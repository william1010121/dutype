//! Shared-block accounting on a scanned tree: sizes with each APFS clone
//! family counted once (`Node::dedup`) and lower bounds of what deleting a
//! folder frees (`Node::freeable`).
//!
//! With `--clones`, the scan records every file with hard links or clone
//! siblings in its dir's `shared` list (without it, nothing runs here). This pass groups them (by inode for hard links, by
//! clone id for clone families), then:
//! - keeps one member of each clone family, picked by a stable rule (the
//!   shortest path, then the smallest), and subtracts the others from the
//!   deduplicated size of every folder above them;
//! - adds a group's bytes to the freeable bytes of the lowest folder that
//!   holds all of it (every inode of the family, every link of each inode),
//!   and of the folders above.
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
    nlink: u64,
    refcnt: u32,
    size: u64,
    /// Position before sorting.
    pos: u32,
}

/// Recompute `dup`, `group_free` and the `Shared` records' `dup` (and
/// unknown `refcnt`) for the whole tree. `base` is the root's path on disk,
/// to read family sizes; without it unknown ones stay unknown.
pub fn account(root: &mut Node, base: Option<&Path>) {
    let mut dirs = Vec::new();
    let mut members = Vec::new();
    collect(root, 0, 0, 0, &mut dirs, &mut members);
    if members.is_empty() {
        return;
    }
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
        for m in g.iter() {
            out[m.pos as usize].1 = m.refcnt;
        }
        // Every link of every inode here, and for a clone family every inode.
        let refcnt = g.iter().map(|m| m.refcnt).max().unwrap_or(0) as usize;
        let all_links = g.chunk_by(|a, b| a.ino == b.ino).all(|l| l.len() as u64 >= l[0].nlink);
        let complete = all_links
            && match is_clone {
                true => refcnt >= 2 && n >= refcnt,
                // A link that may share blocks with another inode: never.
                false => g.iter().all(|m| m.refcnt >= 1),
            };
        if complete {
            // One copy: the bytes counted on the links of one inode.
            let bytes = g
                .chunk_by(|a, b| a.ino == b.ino)
                .map(|l| l.iter().map(|m| m.size).sum::<u64>())
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
    apply(root, &dirs, &mut next, &dup, &free, &out);
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
fn collect(node: &mut Node, parent: u32, child: u32, depth: u32, dirs: &mut Vec<Dir>, members: &mut Vec<Member>) {
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
        });
    }
    for (i, c) in d.children.iter_mut().enumerate() {
        let stale = c.dir.as_ref().is_some_and(|d| d.dup() != 0 || d.group_free() != 0);
        if c.is_dir() && (c.hardlink || c.clone || stale) {
            collect(c, id, i as u32, depth + 1, dirs, members);
        }
    }
}

/// Write the results back, visiting the same dirs as [`collect`].
fn apply(node: &mut Node, dirs: &[Dir], next: &mut usize, dup: &[u64], free: &[u64], out: &[(bool, u32)]) {
    let id = *next;
    *next += 1;
    let d = node.dir.as_deref_mut().expect("a collected dir");
    if dup[id] != 0 || free[id] != 0 || d.clones.is_some() {
        let c = d.clones_mut();
        c.dup = dup[id];
        c.group_free = free[id];
        let first = dirs[id].first as usize;
        for (s, &(is_dup, refcnt)) in c.shared.iter_mut().zip(&out[first..]) {
            s.dup = is_dup;
            s.refcnt = refcnt;
        }
        d.trim_clones();
    }
    while *next < dirs.len() && dirs[*next].parent as usize == id {
        let c = dirs[*next].child as usize;
        apply(&mut d.children[c], dirs, next, dup, free, out);
    }
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
    let d = dir.dir.as_ref().expect("a dir");
    &d.children[d.shared()[m.slot as usize].idx as usize]
}

/// The on-disk path of a member.
fn member_path(root: &Node, dirs: &[Dir], base: &Path, m: &Member) -> Option<PathBuf> {
    let mut path = Vec::new();
    index_path(dirs, m.dir, &mut path);
    let (chain, dir) = dir_chain(root, &path);
    resolve_chain(base, &chain, file_of(dir, m))
}

/// Order of members as family representatives: the shortest path, then the
/// smallest one (name by name), then the smallest inode. Stable across
/// scans, whatever order the parallel scan found them in.
fn compare(root: &Node, dirs: &[Dir], a: &Member, b: &Member, pa: &mut Vec<u32>, pb: &mut Vec<u32>) -> Ordering {
    let depth = |m: &Member| dirs[m.dir as usize].depth;
    depth(a).cmp(&depth(b)).then_with(|| {
        index_path(dirs, a.dir, pa);
        index_path(dirs, b.dir, pb);
        let (mut na, mut nb) = (root, root);
        for (&i, &j) in pa.iter().zip(pb.iter()) {
            na = &na.children()[i as usize];
            nb = &nb.children()[j as usize];
            match na.name.cmp(&nb.name) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        file_of(na, a).name.cmp(&file_of(nb, b).name).then(a.ino.cmp(&b.ino))
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
        assert_eq!(y.freeable(), y.size - MIB);
        // z/plain has a link outside z.
        let z = child(&root, "z");
        assert_eq!(z.freeable(), z.size - MIB);
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
    use super::account;
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
            hardlink: false,
            lossy: false,
            clone: children.iter().any(|c| c.clone),
            dir: Some(Box::new(DirInfo {
                cats: vec![0; reg().len()].into_boxed_slice(),
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
