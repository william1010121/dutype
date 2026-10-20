//! Shared-block accounting on a scanned tree: sizes with each APFS clone
//! family or shared extent counted once (`Node::dedup`) and lower bounds of
//! what deleting a folder frees (`Node::freeable`).
//!
//! With `--clones`, the scan records every file with hard links, clone
//! siblings or (on Linux) shared extents in its dir's `shared` list
//! (without it, nothing runs here). This pass groups them (by inode for
//! hard links, by clone id for clone families), then:
//! - keeps one member of each clone family, picked by a stable rule (the
//!   shortest path, then the smallest), and subtracts the others from the
//!   deduplicated size of every folder above them;
//! - splits shared extents into segments by physical position (a sweep over
//!   the sorted ranges, so partial overlaps work) and keeps each segment at
//!   one of the files that hold it, picked by the same rule; the others
//!   subtract it. Like "Set shared" in `btrfs filesystem du`, every shared
//!   physical byte in the tree is then counted once;
//! - adds a group's bytes to the freeable bytes of the lowest folder that
//!   holds all of it (every inode of the family, every link of each inode),
//!   and of the folders above. Shared extents never count as freeable:
//!   Linux has no global reference count to tell whether every user of an
//!   extent (another file, a snapshot) is inside the tree.
//!
//! A family's size on the whole volume is read here, once per family with
//! two or more members in the tree (one syscall each, in parallel), and kept
//! in its `Shared` records; reading it for every file during the scan made
//! parallel scans twice as slow.
//!
//! It is a pure function of the tree (and those family sizes), so it runs
//! again after a subtree rescan or a cache load.

use crate::scan::{Node, clone_refcnt, resolve_chain};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A visited dir, in pre-order.
struct Dir {
    /// Index of the parent in the dir list (the root is its own parent).
    parent: u32,
    /// Index in the parent's children.
    child: u32,
    depth: u32,
    /// Index of the dir's first `shared` record in the member list, before
    /// sorting (its records follow in order).
    first: u32,
}

struct Member {
    /// Index into the dir list.
    dir: u32,
    /// Index into that dir's `shared`.
    slot: u32,
    key: (u64, u64),
    is_clone: bool,
    ino: u64,
    nlink: u32,
    refcnt: u32,
    size: u64,
    /// Position before sorting.
    pos: u32,
    /// The file has shared extents (Linux `--clones`).
    reflinked: bool,
    /// ... and some of unknown physical size (see `Reflink::opaque`).
    opaque: bool,
}

/// A shared extent of a member: `(device, physical start, length, member
/// position)`.
type Range = (u64, u64, u64, u32);

/// Move each hard-linked inode's already-counted bytes to a deterministic
/// link, before probing shared extents or accounting for clone families.
/// Temporary `Shared` records also exist in default scans, but only for
/// hard links: neither ordinary files nor `Node` gain extra storage.
pub fn attribute_hardlinks(root: &mut Node, clones: bool) {
    if !root.hardlink {
        return;
    }
    let mut links = Hardlinks::default();
    links.collect(root, root, 0, 0, 0);
    let Hardlinks {
        dirs,
        groups,
        mut path_a,
        ..
    } = links;
    for (rep, counted) in groups.into_values() {
        if rep.file == counted.file || counted.size == 0 {
            continue;
        }
        // Remove before adding. Categories may differ because directory
        // rules override extensions; free_base excludes every hard link
        // already and must not change when its allocation moves.
        for (m, size) in [(&counted, 0), (&rep, counted.size)] {
            index_path(&dirs, m.file.dir, &mut path_a);
            let mut dir: &Node = root;
            for &i in &path_a {
                dir = &dir.children()[i as usize];
            }
            let d = dir.dir.as_ref().expect("a dir");
            let idx = d.shared()[m.file.slot as usize].idx as usize;
            let cat = d.children[idx].cat.index();
            let old = d.children[idx].size;
            let mut dir: &mut Node = root;
            for i in path_a.iter().copied().map(Some).chain(std::iter::once(None)) {
                dir.size = dir.size - old + size;
                let d = dir.dir.as_mut().expect("a dir");
                d.cats[cat] = d.cats[cat] - old + size;
                match i {
                    Some(i) => dir = &mut d.children[i as usize],
                    None => {
                        d.children[idx].size = size;
                        break;
                    }
                }
            }
        }
    }
    finish_attribution(root, clones);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileRef {
    dir: u32,
    slot: u32,
}

#[derive(Clone, Copy)]
struct Link {
    file: FileRef,
    size: u64,
}

/// Only two candidates per inode are needed, not a second record for every
/// link. The tree's temporary Shared records retain all links for --clones.
#[derive(Default)]
struct Hardlinks {
    dirs: Vec<Dir>,
    groups: rustc_hash::FxHashMap<(u64, u64), (Link, Link)>,
    path_a: Vec<u32>,
    path_b: Vec<u32>,
}

impl Hardlinks {
    fn collect(&mut self, root: &Node, node: &Node, parent: u32, child: u32, depth: u32) {
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
            let Some(d) = node.dir.as_ref() else { return };
            let id = self.dirs.len() as u32;
            self.dirs.push(Dir {
                parent,
                child,
                depth,
                first: 0,
            });
            for (slot, s) in d.shared().iter().enumerate().filter(|(_, s)| s.nlink > 1) {
                let candidate = Link {
                    file: FileRef {
                        dir: id,
                        slot: slot as u32,
                    },
                    size: d.children[s.idx as usize].size,
                };
                use std::collections::hash_map::Entry;
                match self.groups.entry((s.key.0, s.ino)) {
                    Entry::Vacant(e) => {
                        e.insert((candidate, candidate));
                    }
                    Entry::Occupied(mut e) => {
                        let (rep, counted) = e.get_mut();
                        if compare_files(
                            root,
                            &self.dirs,
                            candidate.file,
                            rep.file,
                            &mut self.path_a,
                            &mut self.path_b,
                        ) == Ordering::Less
                        {
                            *rep = candidate;
                        }
                        if candidate.size > 0 {
                            *counted = candidate;
                        }
                    }
                }
            }
            for (i, c) in d.children.iter().enumerate() {
                if c.is_dir() && c.hardlink {
                    self.collect(root, c, id, i as u32, depth + 1);
                }
            }
        });
    }
}

/// Restore size ordering only after all index paths have been used. Sorting
/// remaps `Shared.idx`; a default scan releases its temporary records first.
fn finish_attribution(node: &mut Node, clones: bool) {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let Some(d) = node.dir.as_mut() else { return };
        for c in &mut d.children {
            if c.hardlink {
                finish_attribution(c, clones);
            }
        }
        if !clones {
            d.clones = None;
        }
        d.sort_children(false);
    });
}

/// Recompute `dup`, `group_free` and the `Shared` records' `dup` (and
/// unknown `refcnt`) for the whole tree. `base` is the root's path on disk,
/// to read family sizes; without it unknown ones stay unknown.
pub fn account(root: &mut Node, base: Option<&Path>) {
    let mut dirs = Vec::new();
    let mut members = Vec::new();
    let mut ranges = Vec::new();
    collect(root, 0, 0, 0, &mut dirs, &mut members, &mut ranges);
    if members.is_empty() {
        return;
    }
    // Per member position: bytes in its shared extents, and the part of
    // them counted at another file. Empty without shared extents.
    let (held, extent_dup) = match ranges.is_empty() {
        true => (Vec::new(), Vec::new()),
        false => extent_dups(root, &dirs, &members, ranges),
    };
    members.sort_unstable_by_key(|m| (m.is_clone, m.key, m.ino));
    let mut groups: Vec<&mut [Member]> = members
        .chunk_by_mut(|a, b| (a.is_clone, a.key) == (b.is_clone, b.key))
        .collect();
    let tree: &Node = root;

    // Family sizes not read yet, for families with two or more inodes here
    // (one inode alone always has a clone outside).
    if let Some(base) = base {
        let wanted: Vec<usize> = (0..groups.len())
            .filter(|&i| {
                let g = &groups[i];
                g[0].is_clone && inodes(g) >= 2 && g.iter().any(|m| m.refcnt == 0)
            })
            .collect();
        let read: Vec<u32> = wanted
            .par_iter()
            .map(|&i| {
                let path = member_path(tree, &dirs, base, &groups[i][0]);
                path.and_then(|p| clone_refcnt(&p)).unwrap_or(1)
            })
            .collect();
        for (i, n) in wanted.into_iter().zip(read) {
            groups[i].iter_mut().for_each(|m| m.refcnt = n);
        }
    }

    let mut dup = vec![0u64; dirs.len()];
    let mut free = vec![0u64; dirs.len()];
    // Per member (by position before sorting): counted elsewhere, refcnt.
    let mut out = vec![(false, 0u32); members_len(&groups)];
    let mut path_a = Vec::new();
    let mut path_b = Vec::new();
    for g in &groups {
        let is_clone = g[0].is_clone;
        let n = inodes(g);
        if is_clone && n >= 2 {
            let mut rep = &g[0];
            for m in &g[1..] {
                if compare(tree, &dirs, m, rep, &mut path_a, &mut path_b) == Ordering::Less {
                    rep = m;
                }
            }
            for m in g.iter().filter(|m| m.ino != rep.ino) {
                dup[m.dir as usize] += m.size;
                out[m.pos as usize].0 = true;
            }
        }
        for m in g.iter().filter(|m| m.reflinked) {
            dup[m.dir as usize] += extent_dup[m.pos as usize];
        }
        for m in g.iter() {
            out[m.pos as usize].1 = m.refcnt;
        }
        // Every link of every inode here, and for a clone family every inode.
        let refcnt = g.iter().map(|m| m.refcnt).max().unwrap_or(0) as usize;
        let all_links = g
            .chunk_by(|a, b| a.ino == b.ino)
            .all(|l| l.len() as u64 >= l[0].nlink as u64);
        let complete = all_links
            && match is_clone {
                true => refcnt >= 2 && n >= refcnt,
                // A link that may share blocks with another inode: never.
                false => g.iter().all(|m| m.refcnt >= 1),
            };
        if complete {
            // One copy: the bytes counted on the links of one inode.
            // Shared extents are never freeable.
            let private = |m: &Member| match m.reflinked {
                true if m.opaque => 0,
                true => m.size.saturating_sub(held[m.pos as usize]),
                false => m.size,
            };
            let bytes = g
                .chunk_by(|a, b| a.ino == b.ino)
                .map(|l| l.iter().map(private).sum::<u64>())
                .max()
                .unwrap_or(0);
            let lca = g[1..].iter().fold(g[0].dir, |a, m| lca(&dirs, a, m.dir));
            free[lca as usize] += bytes;
        }
    }
    // Subtree totals: children come after their parent in pre-order.
    for i in (1..dirs.len()).rev() {
        let p = dirs[i].parent as usize;
        dup[p] += dup[i];
        free[p] += free[i];
    }
    let mut next = 0;
    apply(root, &dirs, &mut next, &dup, &free, &out, &extent_dup);
}

/// Members with shared extents: each shared physical segment stays at one of
/// the files holding it (the first by [`compare`]); every other file holding
/// it does not count it. Returns, per member position, the bytes in its
/// shared extents and the part of them counted at another file.
fn extent_dups(root: &Node, dirs: &[Dir], members: &[Member], mut ranges: Vec<Range>) -> (Vec<u64>, Vec<u64>) {
    // Rank the members with extents by the stable rule (0 = preferred).
    let mut ids: Vec<u32> = ranges.iter().map(|r| r.3).collect();
    ids.dedup();
    let (mut pa, mut pb) = (Vec::new(), Vec::new());
    ids.sort_by(|&a, &b| compare(root, dirs, &members[a as usize], &members[b as usize], &mut pa, &mut pb));
    let mut rank = vec![0u32; members.len()];
    for (r, &id) in ids.iter().enumerate() {
        rank[id as usize] = r as u32;
    }
    let mut held = vec![0u64; members.len()];
    for r in &ranges {
        held[r.3 as usize] += r.2;
    }
    let mut credited = vec![0u64; ids.len()];
    ranges.sort_unstable_by_key(|r| r.0);
    for dev in ranges.chunk_by(|a, b| a.0 == b.0) {
        let segs: Vec<(u64, u64, u32)> = dev.iter().map(|r| (r.1, r.2, rank[r.3 as usize])).collect();
        credit(&segs, &mut credited);
    }
    let mut dup = vec![0u64; members.len()];
    for &id in &ids {
        let i = id as usize;
        dup[i] = held[i].min(members[i].size).saturating_sub(credited[rank[i] as usize]);
    }
    (held, dup)
}

/// The sweep: `ranges` are `(start, len, file)`; every stretch covered by at
/// least one range is credited to the smallest `file` covering it. Adds the
/// credited bytes to `credited[file]`.
pub(crate) fn credit(ranges: &[(u64, u64, u32)], credited: &mut [u64]) {
    let mut events: Vec<(u64, bool, u32)> = Vec::with_capacity(ranges.len() * 2);
    for &(start, len, file) in ranges {
        events.push((start, true, file));
        events.push((start.saturating_add(len), false, file));
    }
    events.sort_unstable_by_key(|e| e.0);
    // Files covering the current position, with how many of their ranges do.
    let mut active: BTreeMap<u32, u32> = BTreeMap::new();
    let mut prev = 0;
    for (pos, open, file) in events {
        if pos > prev
            && let Some((&first, _)) = active.first_key_value()
        {
            credited[first as usize] += pos - prev;
        }
        prev = pos;
        if open {
            *active.entry(file).or_default() += 1;
        } else if let Some(n) = active.get_mut(&file) {
            *n -= 1;
            if *n == 0 {
                active.remove(&file);
            }
        }
    }
}

fn members_len(groups: &[&mut [Member]]) -> usize {
    groups.iter().map(|g| g.len()).sum()
}

/// Inodes in a group (sorted by inode).
fn inodes(g: &[Member]) -> usize {
    g.chunk_by(|a, b| a.ino == b.ino).count()
}

/// Visit the dirs that may hold group members or old results, resetting
/// the results and collecting the members.
fn collect(
    node: &mut Node,
    parent: u32,
    child: u32,
    depth: u32,
    dirs: &mut Vec<Dir>,
    members: &mut Vec<Member>,
    ranges: &mut Vec<Range>,
) {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let Some(d) = node.dir.as_deref_mut() else { return };
        let id = dirs.len() as u32;
        dirs.push(Dir {
            parent,
            child,
            depth,
            first: members.len() as u32,
        });
        if let Some(c) = d.clones.as_deref_mut() {
            c.dup = 0;
            c.group_free = 0;
        }
        for (slot, s) in d.shared().iter().enumerate() {
            for &(start, len) in s.reflink.as_ref().map_or(&[][..], |r| &r.extents) {
                ranges.push((s.key.0, start, len, members.len() as u32));
            }
            members.push(Member {
                dir: id,
                slot: slot as u32,
                key: s.key,
                is_clone: s.is_clone,
                ino: s.ino,
                nlink: s.nlink,
                refcnt: s.refcnt,
                size: d.children[s.idx as usize].size,
                pos: members.len() as u32,
                reflinked: s.reflink.is_some(),
                opaque: s.reflink.as_ref().is_some_and(|r| r.opaque),
            });
        }
        for (i, c) in d.children.iter_mut().enumerate() {
            let stale = c.dir.as_ref().is_some_and(|d| d.dup() != 0 || d.group_free() != 0);
            if c.is_dir() && (c.hardlink || c.clone || stale) {
                collect(c, id, i as u32, depth + 1, dirs, members, ranges);
            }
        }
    });
}

/// Write the results back, visiting the same dirs as [`collect`].
fn apply(
    node: &mut Node,
    dirs: &[Dir],
    next: &mut usize,
    dup: &[u64],
    free: &[u64],
    out: &[(bool, u32)],
    extent_dup: &[u64],
) {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let id = *next;
        *next += 1;
        let d = node.dir.as_deref_mut().expect("a collected dir");
        if dup[id] != 0 || free[id] != 0 || d.clones.is_some() {
            let c = d.clones_mut();
            c.dup = dup[id];
            c.group_free = free[id];
            let first = dirs[id].first as usize;
            for (k, (s, &(is_dup, refcnt))) in c.shared.iter_mut().zip(&out[first..]).enumerate() {
                s.dup = is_dup;
                s.refcnt = refcnt;
                if let Some(r) = &mut s.reflink {
                    r.dup = extent_dup.get(first + k).copied().unwrap_or(0);
                }
            }
            d.trim_clones();
        }
        while *next < dirs.len() && dirs[*next].parent as usize == id {
            let c = dirs[*next].child as usize;
            apply(&mut d.children[c], dirs, next, dup, free, out, extent_dup);
        }
    });
}

/// Child indices from the root down to dir `id`.
fn index_path(dirs: &[Dir], mut id: u32, out: &mut Vec<u32>) {
    out.clear();
    while id != 0 {
        out.push(dirs[id as usize].child);
        id = dirs[id as usize].parent;
    }
    out.reverse();
}

fn lca(dirs: &[Dir], mut a: u32, mut b: u32) -> u32 {
    while a != b {
        let (da, db) = (dirs[a as usize].depth, dirs[b as usize].depth);
        if da >= db {
            a = dirs[a as usize].parent;
        }
        if db >= da {
            b = dirs[b as usize].parent;
        }
    }
    a
}

/// The dir node `id` and its ancestors below the root (outermost first).
fn dir_chain<'a>(root: &'a Node, path: &[u32]) -> (Vec<&'a Node>, &'a Node) {
    let mut chain = Vec::with_capacity(path.len());
    let mut n = root;
    for &i in path {
        n = &n.children()[i as usize];
        chain.push(n);
    }
    (chain, n)
}

fn file_of<'a>(dir: &'a Node, m: &Member) -> &'a Node {
    file_at(dir, m.slot)
}

fn file_at(dir: &Node, slot: u32) -> &Node {
    let d = dir.dir.as_ref().expect("a dir");
    &d.children[d.shared()[slot as usize].idx as usize]
}

/// The on-disk path of a member.
fn member_path(root: &Node, dirs: &[Dir], base: &Path, m: &Member) -> Option<PathBuf> {
    let mut path = Vec::new();
    index_path(dirs, m.dir, &mut path);
    let (chain, dir) = dir_chain(root, &path);
    resolve_chain(base, &chain, file_of(dir, m))
}

/// Escaped display names are injective among invalid UTF-8 names, but a
/// valid name may spell the same escape. Prefer the valid name on that tie.
fn name_order(a: &Node, b: &Node) -> Ordering {
    a.name.cmp(&b.name).then(a.lossy.cmp(&b.lossy))
}

/// Order of members as family representatives: the shortest path, then the
/// smallest one (name by name), then the smallest inode. Stable across
/// scans, whatever order the parallel scan found them in.
fn compare(root: &Node, dirs: &[Dir], a: &Member, b: &Member, pa: &mut Vec<u32>, pb: &mut Vec<u32>) -> Ordering {
    compare_files(
        root,
        dirs,
        FileRef {
            dir: a.dir,
            slot: a.slot,
        },
        FileRef {
            dir: b.dir,
            slot: b.slot,
        },
        pa,
        pb,
    )
    .then(a.ino.cmp(&b.ino))
}

fn compare_files(root: &Node, dirs: &[Dir], a: FileRef, b: FileRef, pa: &mut Vec<u32>, pb: &mut Vec<u32>) -> Ordering {
    let depth = |m: FileRef| dirs[m.dir as usize].depth;
    depth(a).cmp(&depth(b)).then_with(|| {
        index_path(dirs, a.dir, pa);
        index_path(dirs, b.dir, pb);
        let (mut na, mut nb) = (root, root);
        for (&i, &j) in pa.iter().zip(pb.iter()) {
            na = &na.children()[i as usize];
            nb = &nb.children()[j as usize];
            match name_order(na, nb) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        name_order(file_at(na, a.slot), file_at(nb, b.slot))
    })
}

#[cfg(all(test, unix))]
mod tests {
    use crate::scan::{Node, Progress, scan_excluding};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dutype-account-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn child<'a>(n: &'a Node, name: &str) -> &'a Node {
        n.children().iter().find(|c| &*c.name == name).unwrap()
    }

    fn scan(p: &Path) -> Node {
        scan_excluding(p, true, true, &Progress::default(), None).unwrap()
    }

    /// Allocation belongs to the shallowest link, then the smallest path,
    /// not whichever Rayon worker happens to encounter the inode first.
    #[test]
    fn hard_link_attribution_is_stable_across_thread_counts() {
        use std::os::unix::fs::MetadataExt;
        let d = temp("stable-links");
        fs::create_dir_all(d.join("a/deep")).unwrap();
        fs::create_dir_all(d.join("node_modules")).unwrap();
        fs::create_dir_all(d.join("z")).unwrap();
        fs::write(d.join("a/deep/first.bin"), vec![1; 8192]).unwrap();
        for name in ["node_modules/second.js", "z/third.pdf", "root.rs"] {
            fs::hard_link(d.join("a/deep/first.bin"), d.join(name)).unwrap();
        }
        fs::write(d.join("z/last.pdf"), vec![2; 16384]).unwrap();
        fs::hard_link(d.join("z/last.pdf"), d.join("a/first.rs")).unwrap();
        fs::hard_link(d.join("z/last.pdf"), d.join("node_modules/last.js")).unwrap();
        fs::write(d.join("node_modules/alpha.pdf"), vec![3; 4096]).unwrap();
        fs::hard_link(d.join("node_modules/alpha.pdf"), d.join("z/alpha.rs")).unwrap();
        let special_bytes = fs::metadata(d.join("node_modules/alpha.pdf")).unwrap().blocks() * 512;
        let root_bytes = fs::metadata(d.join("root.rs")).unwrap().blocks() * 512;
        let sibling_bytes = fs::metadata(d.join("a/first.rs")).unwrap().blocks() * 512;
        for threads in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            for clones in [false, true] {
                for _ in 0..8 {
                    let root = pool.install(|| scan_excluding(&d, true, clones, &Progress::default(), None).unwrap());
                    assert_eq!(child(&root, "root.rs").size, root_bytes);
                    assert_eq!(child(child(&root, "a"), "first.rs").size, sibling_bytes);
                    for (dir, file) in [
                        ("node_modules", "second.js"),
                        ("node_modules", "last.js"),
                        ("z", "third.pdf"),
                        ("z", "last.pdf"),
                    ] {
                        assert_eq!(child(child(&root, dir), file).size, 0);
                    }
                    assert_eq!(child(child(child(&root, "a"), "deep"), "first.bin").size, 0);
                    fn check(n: &Node) {
                        if let Some(dir) = &n.dir {
                            assert_eq!(dir.cats.iter().sum::<u64>(), n.size);
                            for c in &dir.children {
                                check(c);
                            }
                        }
                    }
                    check(&root);
                    let modules = child(&root, "node_modules");
                    assert_eq!(child(modules, "alpha.pdf").size, special_bytes);
                    assert_eq!(child(modules, "alpha.pdf").cat, modules.cat);
                    assert_eq!(child(child(&root, "z"), "alpha.rs").size, 0);
                    let code = child(&root, "root.rs").cat.index();
                    let cats = root.cats();
                    assert_eq!(cats[code], root_bytes + sibling_bytes);
                    let own_modules = fs::metadata(d.join("node_modules")).unwrap().blocks() * 512;
                    assert_eq!(cats[modules.cat.index()], own_modules + special_bytes);
                    let own = ["", "a", "a/deep", "node_modules", "z"]
                        .iter()
                        .map(|p| fs::metadata(d.join(p)).unwrap().blocks() * 512)
                        .sum::<u64>();
                    assert_eq!(root.size, own + root_bytes + sibling_bytes + special_bytes);
                    assert_eq!(root.dedup(), root.size);
                    assert_eq!(
                        root.freeable(),
                        root.size
                            - if clones {
                                0
                            } else {
                                root_bytes + sibling_bytes + special_bytes
                            }
                    );
                }
            }
        }
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn hard_link_names_with_identical_display_have_a_stable_order() {
        use std::ffi::OsStr;
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        let d = temp("lossy-links");
        let raw = OsStr::from_bytes(b"\xff.rs");
        // APFS rejects non-UTF-8 filenames. Skip only that explicit filesystem
        // limitation; synthetic name-tie coverage still runs on every platform.
        match fs::write(d.join(raw), vec![1; 4096]) {
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {
                fs::remove_dir_all(d).unwrap();
                return;
            }
            result => result.unwrap(),
        }
        // The invalid byte's escaped display is the valid file's literal name.
        fs::hard_link(d.join(raw), d.join(Path::new("\\xff.rs"))).unwrap();
        let bytes = fs::metadata(d.join(raw)).unwrap().blocks() * 512;
        let raw_dir = OsStr::from_bytes(b"\xfe");
        fs::create_dir(d.join(raw_dir)).unwrap();
        fs::create_dir(d.join(Path::new("\\xfe"))).unwrap();
        fs::write(d.join(raw_dir).join("nested.rs"), vec![2; 8192]).unwrap();
        fs::hard_link(
            d.join(raw_dir).join("nested.rs"),
            d.join(Path::new("\\xfe")).join("nested.rs"),
        )
        .unwrap();
        let nested = fs::metadata(d.join(raw_dir).join("nested.rs")).unwrap().blocks() * 512;
        for threads in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            for clones in [false, true] {
                for _ in 0..4 {
                    let root = pool.install(|| scan_excluding(&d, true, clones, &Progress::default(), None).unwrap());
                    for c in root.children() {
                        if c.is_dir() {
                            assert_eq!(child(c, "nested.rs").size, if c.lossy { 0 } else { nested });
                        } else {
                            assert_eq!(c.size, if c.lossy { 0 } else { bytes });
                        }
                    }
                }
            }
        }
        fs::remove_dir_all(d).unwrap();
    }

    /// A hard link frees its blocks only where every link is inside.
    #[test]
    fn hard_links_free_where_all_links_are_inside() {
        let d = temp("links");
        fs::create_dir_all(d.join("x/y")).unwrap();
        fs::create_dir_all(d.join("z")).unwrap();
        fs::write(d.join("x/f"), vec![1u8; 1 << 20]).unwrap();
        fs::hard_link(d.join("x/f"), d.join("x/y/g")).unwrap();
        fs::write(d.join("z/plain"), vec![1u8; 1 << 20]).unwrap();
        fs::hard_link(d.join("z/plain"), d.join("outside")).unwrap();
        const MIB: u64 = 1 << 20;
        let root = scan(&d);
        // Dirs' own blocks (0 on APFS, a block each on ext4) are always freed.
        let x = child(&root, "x");
        assert_eq!(x.freeable(), x.size);
        let y = child(x, "y");
        assert_eq!(y.freeable(), y.size);
        // z/plain has a link outside z.
        let z = child(&root, "z");
        assert_eq!(z.freeable(), z.size);
        assert_eq!(root.freeable(), root.size);
        // Hard links are never "dedup" savings: they are counted once already.
        assert_eq!(root.dedup(), root.size);

        // Scanned alone, x/y holds one of two links.
        let y = scan(&d.join("x/y"));
        assert_eq!(y.freeable(), y.size - MIB);

        // Without --clones no groups are recorded or accounted: a hard link
        // never counts as freeable, even with every link inside.
        let root = scan_excluding(&d, true, false, &Progress::default(), None).unwrap();
        fn no_records(n: &Node) -> bool {
            n.dir
                .as_ref()
                .is_none_or(|d| d.clones.is_none() && d.children.iter().all(no_records))
        }
        assert!(no_records(&root));
        let x = child(&root, "x");
        assert_eq!(x.freeable(), x.size - MIB);
        assert_eq!(root.freeable(), root.size - 2 * MIB);
        assert_eq!(root.dedup(), root.size);
        fs::remove_dir_all(&d).unwrap();
    }
}

/// Hand-built trees, so the clone rules are tested on every platform.
#[cfg(test)]
mod synthetic {
    use super::{account, attribute_hardlinks};
    use crate::category::reg;
    use crate::scan::{CloneInfo, DirInfo, Node, Shared};
    use std::path::Path;

    const S: u64 = 4096;

    fn file(name: &str, clone: bool) -> Node {
        Node {
            name: name.into(),
            size: S,
            cat: reg().other(),
            special: false,
            hardlink: false,
            lossy: false,
            clone,
            dir: None,
        }
    }

    /// A dir whose files named `c*` are members of one clone family.
    fn dir(name: &str, children: Vec<Node>) -> Node {
        let shared: Vec<Shared> = children
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_dir() && c.name.starts_with('c'))
            .map(|(i, c)| Shared {
                idx: i as u32,
                key: (1, 100),
                is_clone: true,
                ino: c.name.bytes().map(u64::from).sum(),
                nlink: 1,
                refcnt: 0,
                dup: false,
                reflink: None,
            })
            .collect();
        let size = children.iter().map(|c| c.size).sum();
        let free_base = children
            .iter()
            .map(|c| c.dir.as_ref().map_or(if c.clone { 0 } else { c.size }, |d| d.free_base))
            .sum();
        Node {
            name: name.into(),
            size,
            cat: reg().other(),
            special: false,
            hardlink: children.iter().any(|c| c.hardlink),
            lossy: false,
            clone: children.iter().any(|c| c.clone),
            dir: Some(Box::new(DirInfo {
                cats: {
                    let mut cats = vec![0; reg().len()].into_boxed_slice();
                    cats[reg().other().index()] = size;
                    cats
                },
                files: children.iter().map(Node::files).sum(),
                errors: 0,
                children,
                free_base,
                clones: Some(Box::new(CloneInfo {
                    shared: shared.into_boxed_slice(),
                    ..CloneInfo::default()
                })),
            })),
        }
    }

    /// root/{c9, plain, d/{cb, ca}}: one family of three, at depths 1 and 2.
    fn tree(flip: bool) -> Node {
        let mut inner = vec![file("cb", true), file("ca", true)];
        if flip {
            inner.reverse();
        }
        dir("root", vec![dir("d", inner), file("c9", true), file("plain", false)])
    }

    fn set_refcnt(n: &mut Node, refcnt: u32) {
        if let Some(d) = n.dir.as_deref_mut() {
            d.clones_mut().shared.iter_mut().for_each(|s| s.refcnt = refcnt);
            d.children.iter_mut().for_each(|c| set_refcnt(c, refcnt));
        }
    }

    fn dups(n: &Node) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        if let Some(d) = &n.dir {
            for s in d.shared() {
                out.push((d.children[s.idx as usize].name.to_string(), s.dup));
            }
            d.children.iter().for_each(|c| out.extend(dups(c)));
        }
        out.sort();
        out
    }

    #[test]
    fn hard_link_transfer_precedes_clone_accounting_without_filesystem_access() {
        for flip in [false, true] {
            let mut shallow = file("c_a", true);
            shallow.hardlink = true;
            shallow.size = 0;
            let mut deep = file("c_z", true);
            deep.hardlink = true;
            let mut children = vec![dir("nested", vec![deep]), shallow, file("c_b", true)];
            if flip {
                children.reverse();
            }
            let mut root = dir("nonexistent-root", children);
            fn identities(n: &mut Node) {
                if let Some(d) = n.dir.as_mut() {
                    for s in &mut d.clones.as_mut().unwrap().shared {
                        let c = &d.children[s.idx as usize];
                        s.ino = if c.hardlink { 10 } else { 20 };
                        s.nlink = if c.hardlink { 2 } else { 1 };
                        s.refcnt = 2;
                    }
                    for c in &mut d.children {
                        identities(c);
                    }
                }
            }
            identities(&mut root);
            for _ in 0..2 {
                attribute_hardlinks(&mut root, true);
                account(&mut root, None);
                assert_eq!(root.size, 2 * S);
                assert_eq!(root.cats()[reg().other().index()], 2 * S);
                assert_eq!(root.dedup(), S);
                assert_eq!(root.freeable(), S);
                let d = root.dir.as_ref().unwrap();
                for (i, c) in d.children.iter().enumerate() {
                    match &*c.name {
                        "c_a" => assert_eq!((c.size, d.child_dedup(i)), (S, S)),
                        "c_b" => assert_eq!((c.size, d.child_dedup(i)), (S, 0)),
                        "nested" => assert_eq!((c.size, c.freeable(), c.cats()[reg().other().index()]), (0, 0, 0)),
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    #[test]
    fn clone_display_name_ties_prefer_valid_names() {
        for flip in [false, true] {
            let mut invalid = file("c\\xff", true);
            invalid.lossy = true;
            let valid = file("c\\xff", true);
            let mut children = vec![invalid, valid];
            if flip {
                children.reverse();
            }
            let mut root = dir("root", children);
            let d = root.dir.as_mut().unwrap();
            for (i, s) in d.clones.as_mut().unwrap().shared.iter_mut().enumerate() {
                s.ino = i as u64;
                s.refcnt = 2;
            }
            account(&mut root, None);
            let d = root.dir.as_ref().unwrap();
            for (i, c) in d.children.iter().enumerate() {
                assert_eq!(d.child_dedup(i), if c.lossy { 0 } else { S });
            }
        }
    }

    #[test]
    fn family_is_counted_once_at_the_shortest_path() {
        for flip in [false, true] {
            let mut root = tree(flip);
            account(&mut root, None);
            assert_eq!(root.size, 4 * S);
            assert_eq!(root.dedup(), 2 * S, "flip {flip}");
            // c9 (depth 1) keeps the bytes; both copies in d are dropped.
            let want = [("c9", false), ("ca", true), ("cb", true)].map(|(n, d)| (n.to_string(), d));
            assert_eq!(dups(&root), want);
            assert_eq!(root.children()[0].dedup(), 0);
        }
    }

    #[test]
    fn family_frees_only_with_a_known_complete_count() {
        // Unknown family size (no path to read it from): never complete.
        let mut root = tree(false);
        account(&mut root, None);
        assert_eq!(root.freeable(), S);

        // Three members, all here: one copy is freed by deleting the root,
        // nothing by deleting d (c9 still holds the blocks).
        set_refcnt(&mut root, 3);
        account(&mut root, None);
        assert_eq!(root.freeable(), 2 * S);
        assert_eq!(root.children()[0].freeable(), 0);

        // A fourth member elsewhere.
        set_refcnt(&mut root, 4);
        account(&mut root, None);
        assert_eq!(root.freeable(), S);
    }

    /// A family size that cannot be read is stored as 1 (never complete)
    /// so it is not read again on every pass.
    #[test]
    fn unreadable_family_size_is_not_read_again() {
        let mut root = tree(false);
        account(&mut root, Some(Path::new("/nonexistent-dutype-root")));
        assert_eq!(root.freeable(), S);
        let d = root.children()[0].dir.as_ref().unwrap();
        assert!(d.shared().iter().all(|s| s.refcnt == 1));
    }

    /// A hard-linked file that may share blocks with another inode (a
    /// partial clone) frees nothing for sure, even with all links inside.
    #[test]
    fn hard_link_that_may_share_is_never_freeable() {
        for (refcnt, freed) in [(1, S), (0, 0)] {
            let mut h = file("h", true);
            h.hardlink = true;
            let mut root = dir("root", vec![h]);
            let d = root.dir.as_deref_mut().unwrap();
            d.clones_mut().shared = vec![Shared {
                idx: 0,
                key: (1, 7),
                is_clone: false,
                ino: 7,
                nlink: 1,
                refcnt,
                dup: false,
                reflink: None,
            }]
            .into_boxed_slice();
            account(&mut root, None);
            assert_eq!(root.freeable(), freed, "refcnt {refcnt}");
        }
    }

    /// Results of an earlier pass do not leak into the next one.
    #[test]
    fn account_is_idempotent() {
        let mut root = tree(false);
        set_refcnt(&mut root, 3);
        account(&mut root, None);
        let first = (root.dedup(), root.freeable(), dups(&root));
        account(&mut root, None);
        assert_eq!((root.dedup(), root.freeable(), dups(&root)), first);
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::credit;

    fn run(ranges: &[(u64, u64, u32)], files: usize) -> Vec<u64> {
        let mut c = vec![0; files];
        credit(ranges, &mut c);
        c
    }

    #[test]
    fn disjoint_ranges_stay_with_their_file() {
        assert_eq!(run(&[(0, 10, 1), (20, 5, 0)], 2), [5, 10]);
        assert_eq!(run(&[], 1), [0]);
    }

    #[test]
    fn the_first_file_keeps_shared_segments() {
        // Two full copies and one partial one of [100, 200).
        assert_eq!(run(&[(100, 100, 2), (100, 100, 1), (150, 50, 0)], 3), [50, 50, 0]);
        // Partial overlaps: 2 keeps what nobody earlier holds.
        assert_eq!(run(&[(0, 60, 2), (40, 40, 0), (70, 50, 1)], 3), [40, 40, 40]);
    }

    #[test]
    fn touching_and_repeated_ranges() {
        // Touching ranges of one file, a range a file holds twice.
        assert_eq!(run(&[(0, 10, 0), (10, 10, 0), (5, 10, 1)], 2), [20, 0]);
        assert_eq!(run(&[(0, 10, 1), (0, 10, 1)], 2), [0, 10]);
        // Every covered byte is credited exactly once.
        let r = [(3, 9, 0), (0, 4, 1), (8, 8, 2), (20, 1, 1), (15, 6, 0)];
        assert_eq!(run(&r, 3).iter().sum::<u64>(), 16 + 5);
    }
}

#[cfg(all(test, unix))]
mod extent_tests {
    use crate::scan::{Node, Progress, Reflink, Shared, scan_excluding};
    use std::fs;

    /// Mark the file at `path` as holding the shared `extents`, as a
    /// `--clones` scan on btrfs or XFS would.
    fn reflink(root: &mut Node, path: &[&str], extents: &[(u64, u64)], opaque: bool) {
        let (file, dirs) = path.split_last().unwrap();
        let mut n = &mut *root;
        let mut chain = vec![];
        for name in dirs {
            let i = n.children().iter().position(|c| &*c.name == *name).unwrap();
            chain.push(i);
            n = &mut n.dir.as_mut().unwrap().children[i];
        }
        let d = n.dir.as_mut().unwrap();
        let i = d.children.iter().position(|c| &*c.name == *file).unwrap();
        let size = d.children[i].size;
        d.children[i].clone = true;
        let mut shared = d.shared().to_vec();
        shared.push(Shared {
            idx: i as u32,
            key: (1, i as u64 + 1000 * chain.len() as u64),
            is_clone: false,
            ino: i as u64 + 1000 * chain.len() as u64,
            nlink: 1,
            refcnt: 1,
            dup: false,
            reflink: Some(Box::new(Reflink {
                extents: extents.into(),
                dup: 0,
                opaque,
            })),
        });
        shared.sort_by_key(|s| s.idx);
        d.clones_mut().shared = shared.into_boxed_slice();
        // The scan leaves such files out of the plain freeable bytes.
        let mut n = &mut *root;
        n.clone = true;
        n.dir.as_mut().unwrap().free_base -= size;
        for i in chain {
            n = &mut n.dir.as_mut().unwrap().children[i];
            n.clone = true;
            n.dir.as_mut().unwrap().free_base -= size;
        }
    }

    #[test]
    fn shared_extents_count_once_at_the_first_path() {
        const M: u64 = 1 << 20;
        let d = std::env::temp_dir().join(format!("dutype-account-extents-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        for f in ["a", "c", "sub/b", "plain"] {
            fs::write(d.join(f), vec![1u8; 8 << 20]).unwrap();
        }
        let mut root = scan_excluding(&d, true, true, &Progress::default(), None).unwrap();
        let own = root.size - 32 * M;
        let plain = root.freeable() - 32 * M;
        let p = 1 << 40;
        // a and sub/b: full copies of one extent; c: its last 7 MiB plus 1 MiB of its own.
        reflink(&mut root, &["a"], &[(p, 8 * M)], false);
        reflink(&mut root, &["c"], &[(p + M, 7 * M)], false);
        reflink(&mut root, &["sub", "b"], &[(p, 8 * M)], false);
        crate::account::account(&mut root, None);

        assert_eq!(root.size, own + 32 * M);
        // a keeps the extent: c's 7 MiB and b's 8 MiB are copies.
        assert_eq!(root.dedup(), own + 17 * M);
        let dir = root.dir.as_ref().unwrap();
        let at = |name: &str| dir.children.iter().position(|c| &*c.name == name).unwrap();
        assert_eq!(dir.child_dedup(at("a")), 8 * M);
        assert_eq!(dir.child_dedup(at("c")), M);
        assert_eq!(dir.child_dedup(at("plain")), 8 * M);
        let sub = &dir.children[at("sub")];
        assert_eq!(sub.dedup(), sub.size - 8 * M);
        // Only bytes in no shared extent are freeable.
        assert_eq!(dir.child_freeable(at("a")), 0);
        assert_eq!(dir.child_freeable(at("c")), M);
        assert_eq!(dir.child_freeable(at("plain")), 8 * M);
        assert_eq!(root.freeable(), plain + 8 * M + M);
        assert_eq!(sub.freeable(), sub.size - 8 * M);

        // A pure function of the tree: running it again changes nothing.
        crate::account::account(&mut root, None);
        assert_eq!(root.dedup(), own + 17 * M);

        // "plain" also shares compressed extents (unknown physical size): it
        // keeps its readable shared range but nothing of it is freeable.
        reflink(&mut root, &["plain"], &[(p + 7 * M, M)], true);
        crate::account::account(&mut root, None);
        let dir = root.dir.as_ref().unwrap();
        let i = dir.children.iter().position(|c| &*c.name == "plain").unwrap();
        assert_eq!(dir.child_freeable(i), 0);
        assert_eq!(dir.child_dedup(i), 7 * M);
        assert_eq!(root.freeable(), plain + M);
        fs::remove_dir_all(&d).unwrap();
    }
}
