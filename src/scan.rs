use crate::category::{Category, reg};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct Node {
    pub name: Box<str>,
    /// Allocated bytes on disk (like `du`).
    pub size: u64,
    /// Files: their category. Dirs: the special category they belong to, or Other.
    pub cat: Category,
    /// Dir itself is the root of a special dir (node_modules, .venv, ...).
    pub special: bool,
    /// File: it has more than one link. Dir: some file below it has. Such
    /// subtrees cannot be rescanned alone without risking double counting.
    pub hardlink: bool,
    /// The name on disk is not valid UTF-8; `name` is a lossy rendering
    /// (which may even collide with a sibling's real name).
    pub lossy: bool,
    /// File: it may share blocks with another file (an APFS clone, now or
    /// once), so deleting it alone may free less than its size. Dir: some
    /// file below it may.
    pub clone: bool,
    pub dir: Option<Box<DirInfo>>,
}

pub struct DirInfo {
    /// Bytes per category, indexed by `Category::index`.
    pub cats: Box<[u64]>,
    pub files: u64,
    /// Unreadable entries in this subtree, so a subtree rescan can replace
    /// its share of the total instead of adding to it.
    pub errors: u64,
    pub children: Vec<Node>,
    /// "Freeable" bytes that need no shared-block bookkeeping: the dirs' own
    /// blocks and files without hard links or clones, in this subtree.
    pub free_base: u64,
    /// Shared-block state, only with --clones (and only where it is not
    /// all empty), so a default scan costs one pointer per dir for it.
    pub clones: Option<Box<CloneInfo>>,
}

/// The part of a [`DirInfo`] that only clone detection (`--clones`) uses.
#[derive(Default)]
pub struct CloneInfo {
    /// Bytes of complete groups (every link and clone of them inside) whose
    /// lowest common ancestor is in this subtree. Set by [`crate::account`].
    pub group_free: u64,
    /// Bytes of clones in this subtree whose family is already counted at
    /// another member. Set by [`crate::account`].
    pub dup: u64,
    /// Files of this dir with hard links or clone siblings, by child index.
    pub shared: Box<[Shared]>,
    /// Files in this subtree whose clone flags could not be read, so a
    /// subtree rescan can replace its share of the gaps.
    pub gaps: u64,
}

impl CloneInfo {
    fn is_empty(&self) -> bool {
        self.group_free == 0 && self.dup == 0 && self.shared.is_empty() && self.gaps == 0
    }
}

/// A file of a [`DirInfo`] that shares blocks with other files: hard links
/// (same inode) or members of an APFS full-clone family (same clone id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shared {
    /// Index of the file in `children`.
    pub idx: u32,
    /// The group: `(dev, clone id)` for a clone family, else `(dev, inode)`.
    pub key: (u64, u64),
    pub is_clone: bool,
    pub ino: u64,
    pub nlink: u64,
    /// Inodes in the clone family on the whole volume, read by
    /// [`crate::account`] once the family has two members in the tree: 0
    /// while unknown, 1 when it cannot be read (the family is then never
    /// complete). For a hard link: 1, or 0 when it may share blocks with
    /// another inode (then its group is never complete).
    pub refcnt: u32,
    /// Set by [`crate::account`]: the family is counted at another member,
    /// so this file adds nothing to deduplicated sizes.
    pub dup: bool,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.dir.is_some()
    }

    pub fn children(&self) -> &[Node] {
        self.dir.as_ref().map_or(&[], |d| d.children.as_slice())
    }

    /// Unreadable entries in this subtree.
    pub fn errors(&self) -> u64 {
        self.dir.as_ref().map_or(0, |d| d.errors)
    }

    /// Files in this subtree whose clone flags could not be read.
    pub fn clone_gaps(&self) -> u64 {
        self.dir.as_ref().map_or(0, |d| d.clone_gaps())
    }

    pub fn files(&self) -> u64 {
        self.dir.as_ref().map_or(1, |d| d.files)
    }

    /// Size with each APFS clone family counted once (additive).
    pub fn dedup(&self) -> u64 {
        self.dir
            .as_ref()
            .map_or(self.size, |d| self.size.saturating_sub(d.dup()))
    }

    /// Lower bound of the bytes deleting this would free (not additive).
    pub fn freeable(&self) -> u64 {
        match &self.dir {
            Some(d) => d.free_base + d.group_free(),
            None if self.hardlink || self.clone => 0,
            None => self.size,
        }
    }

    pub fn cats(&self) -> Box<[u64]> {
        match &self.dir {
            Some(d) => d.cats.clone(),
            None => {
                let mut c = vec![0; reg().len()].into_boxed_slice();
                c[self.cat.index()] = self.size;
                c
            }
        }
    }
}

impl DirInfo {
    /// Files of this dir with hard links or clone siblings (`--clones`).
    pub fn shared(&self) -> &[Shared] {
        self.clones.as_ref().map_or(&[], |c| &c.shared)
    }

    /// See [`CloneInfo::dup`].
    pub fn dup(&self) -> u64 {
        self.clones.as_ref().map_or(0, |c| c.dup)
    }

    /// See [`CloneInfo::group_free`].
    pub fn group_free(&self) -> u64 {
        self.clones.as_ref().map_or(0, |c| c.group_free)
    }

    /// See [`CloneInfo::gaps`].
    pub fn clone_gaps(&self) -> u64 {
        self.clones.as_ref().map_or(0, |c| c.gaps)
    }

    /// The clone state, created empty if there is none yet.
    pub fn clones_mut(&mut self) -> &mut CloneInfo {
        self.clones.get_or_insert_with(Box::default)
    }

    /// Drop the clone state if nothing is left in it.
    pub fn trim_clones(&mut self) {
        if self.clones.as_ref().is_some_and(|c| c.is_empty()) {
            self.clones = None;
        }
    }

    /// The [`Shared`] record of child `i`, if it has one.
    pub fn shared_of(&self, i: usize) -> Option<&Shared> {
        let shared = self.shared();
        shared
            .binary_search_by_key(&(i as u32), |s| s.idx)
            .ok()
            .map(|k| &shared[k])
    }

    /// Deduplicated size of child `i` (see [`Node::dedup`]).
    pub fn child_dedup(&self, i: usize) -> u64 {
        let c = &self.children[i];
        match self.shared_of(i) {
            Some(s) if s.dup => 0,
            _ => c.dedup(),
        }
    }

    /// Sort children biggest first by `key` (then by name), keeping
    /// `shared` pointing at the same files.
    pub fn sort_children(&mut self, by_dedup: bool) {
        if self.shared().is_empty() && !by_dedup {
            self.children
                .sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
            return;
        }
        let n = self.children.len();
        let keys: Vec<u64> = match by_dedup {
            true => (0..n).map(|i| self.child_dedup(i)).collect(),
            false => Vec::new(),
        };
        let key = |i: usize| keys.get(i).copied().unwrap_or(self.children[i].size);
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            key(b)
                .cmp(&key(a))
                .then_with(|| self.children[a].name.cmp(&self.children[b].name))
        });
        let mut new_pos = vec![0u32; n];
        for (pos, &old) in order.iter().enumerate() {
            new_pos[old as usize] = pos as u32;
        }
        if let Some(c) = self.clones.as_deref_mut() {
            for s in c.shared.iter_mut() {
                s.idx = new_pos[s.idx as usize];
            }
            c.shared.sort_unstable_by_key(|s| s.idx);
        }
        // Apply the permutation in place, one cycle at a time (position
        // `pos` takes the child at `order[pos]`).
        for start in 0..n {
            let mut pos = start;
            loop {
                let src = std::mem::replace(&mut order[pos], pos as u32) as usize;
                if src == start {
                    break;
                }
                self.children.swap(pos, src);
                pos = src;
            }
        }
    }
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    /// Some directory reported APFS clone flags.
    pub clone_ids: AtomicBool,
    /// With --clones, some files' clone flags could not be read.
    pub clone_gaps: AtomicBool,
    /// Set to stop a running scan early (the result is then incomplete).
    pub cancel: AtomicBool,
    /// Request (`--private`): read the APFS private size of partial clones.
    /// One extra syscall per such file, so it is opt-in.
    pub private: bool,
    /// Some directory was read in a way that reports private sizes (macOS,
    /// bulk reads, with `private`).
    pub private_sizes: AtomicBool,
    /// Some volume did not report the private size of a partial clone.
    pub private_missing: AtomicBool,
    /// The private sizes found, by path relative to the scanned root.
    pub found: Mutex<Vec<(PathBuf, u64)>>,
}

impl Progress {
    /// The private sizes the scan found (see [`PrivateSizes`]).
    pub fn take_private(&self) -> PrivateSizes {
        PrivateSizes(std::mem::take(&mut *self.found.lock().unwrap()).into_iter().collect())
    }
}

/// APFS private sizes (`--private`) of partial clones: what deleting that
/// file alone frees right now, by its path relative to the scanned root
/// (the tree's names, so it matches [`walk`] paths). Kept beside the tree,
/// so a scan without `--private` costs no memory for it. Never summed into
/// folders.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrivateSizes(pub FxHashMap<PathBuf, u64>);

impl PrivateSizes {
    pub fn get(&self, rel: &Path) -> Option<u64> {
        self.0.get(rel).copied()
    }

    /// Replace everything below `prefix` (a rescanned folder) with `fresh`,
    /// whose paths are relative to that folder.
    pub fn replace_below(&mut self, prefix: &Path, fresh: PrivateSizes) {
        self.0.retain(|p: &PathBuf, _| !p.starts_with(prefix));
        self.0.extend(fresh.0.into_iter().map(|(p, b)| (prefix.join(p), b)));
    }
}

#[cfg(unix)]
mod platform {
    use super::{Entry, ReadStats};
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    #[cfg(target_os = "macos")]
    pub(super) mod bulk;

    /// Entries of the directory `path`: from `getattrlistbulk` on macOS when
    /// the volume supports it, else [`super::read_entries_std`].
    /// With `clones`, also APFS clone identity (macOS); without it, the same
    /// request as a plain scan. `private` (with `clones`) also asks for
    /// [`ReadStats::private`] (bulk reads only).
    pub fn read_entries(
        path: &Path,
        cancel: &AtomicBool,
        out: &mut Vec<Entry>,
        clones: bool,
        private: bool,
    ) -> io::Result<ReadStats> {
        #[cfg(target_os = "macos")]
        if let Some(r) = bulk::read_entries(path, cancel, out, clones, private) {
            return r;
        }
        let first = out.len();
        let _ = private;
        let mut r = super::read_entries_std(path, cancel, out);
        // On macOS any volume may share blocks (APFS clones, server-side
        // copies); without bulk flags a file's clone state is unknown.
        if clones && cfg!(target_os = "macos") {
            let gaps = unknown_clone_state(&mut out[first..]);
            if let Ok(stats) = &mut r {
                stats.clone_gaps += gaps;
            }
        }
        r
    }

    /// Files whose clone state could not be read: they may share blocks, so
    /// they are never counted as freeable. Returns how many.
    pub fn unknown_clone_state(entries: &mut [Entry]) -> u64 {
        let mut n = 0;
        for e in entries.iter_mut().filter(|e| !e.dir) {
            e.may_share = true;
            n += 1;
        }
        n
    }

    /// Files in the APFS full-clone family of the file at `path`, where the
    /// system can tell (macOS 14+).
    pub fn clone_refcnt(path: &Path) -> Option<u32> {
        #[cfg(target_os = "macos")]
        return bulk::clone_refcnt(path);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            None
        }
    }

    /// Files on the filesystem of `root` may share blocks (APFS clones,
    /// reflinks on btrfs, XFS, bcachefs, OCFS2 or ZFS block cloning), so
    /// without shared-block detection nothing is known to be freeable.
    pub fn may_share_blocks(root: &Path) -> bool {
        #[cfg(target_os = "macos")]
        {
            let _ = root;
            true
        }
        #[cfg(target_os = "linux")]
        {
            const REFLINK_FS: [u64; 5] = [
                0x9123_683e, // btrfs
                0x5846_5342, // XFS
                0xca45_1a4e, // bcachefs
                0x7461_636f, // OCFS2
                0x2fc1_2fc1, // ZFS
            ];
            let Ok(cpath) = std::ffi::CString::new(root.as_os_str().as_bytes()) else {
                return true;
            };
            let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::statfs(cpath.as_ptr(), st.as_mut_ptr()) } != 0 {
                return true;
            }
            let kind = unsafe { st.assume_init() }.f_type as u64 & 0xffff_ffff;
            REFLINK_FS.contains(&kind)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = root;
            false
        }
    }

    /// The file at `path` may share blocks with another (an APFS clone).
    /// `None` when that cannot be read.
    pub fn may_share(path: &Path) -> Option<bool> {
        #[cfg(target_os = "macos")]
        return bulk::may_share(path);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            None
        }
    }

    pub fn allocated(m: &Metadata) -> u64 {
        m.blocks() * 512
    }
    pub fn device(m: &Metadata) -> u64 {
        m.dev()
    }
    /// Identity of a hard-linked file, if it has more than one link.
    pub fn hardlink_id(m: &Metadata) -> Option<(u64, u64)> {
        (m.nlink() > 1 && !m.is_dir()).then(|| (m.dev(), m.ino()))
    }
    /// `(inode, link count)`.
    pub fn file_id(m: &Metadata) -> (u64, u64) {
        (m.ino(), m.nlink())
    }

    /// Escaped rendering of a non-UTF-8 name, like `ls -b` (`\\xff` for bad
    /// bytes, `\\\\` for backslashes): distinct raw names stay distinct and
    /// [`lossy_child`] recovers the exact bytes.
    pub fn lossy_name(raw: &OsStr) -> String {
        escape(raw.as_bytes())
    }

    /// `base` joined with the raw name behind a [`lossy_name`].
    pub fn lossy_child(base: &Path, name: &str) -> Option<PathBuf> {
        Some(base.join(OsStr::from_bytes(&unescape(name)?)))
    }

    pub fn escape(raw: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(raw.len() + 8);
        for chunk in raw.utf8_chunks() {
            for c in chunk.valid().chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    c => out.push(c),
                }
            }
            for b in chunk.invalid() {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
        out
    }

    /// Inverse of [`escape`]; `None` for a string it could not have produced.
    pub fn unescape(name: &str) -> Option<Vec<u8>> {
        let b = name.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match (b[i], b.get(i + 1)) {
                (b'\\', Some(b'\\')) => {
                    out.push(b'\\');
                    i += 2;
                }
                (b'\\', Some(b'x')) => {
                    let hex = std::str::from_utf8(b.get(i + 2..i + 4)?).ok()?;
                    out.push(u8::from_str_radix(hex, 16).ok()?);
                    i += 4;
                }
                (b'\\', _) => return None,
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        Some(out)
    }
}

#[cfg(not(unix))]
mod platform {
    use super::{Entry, ReadStats};
    use std::ffi::OsStr;
    use std::fs::{self, Metadata};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    /// No clone identity or private sizes here.
    pub fn read_entries(
        path: &Path,
        cancel: &AtomicBool,
        out: &mut Vec<Entry>,
        _clones: bool,
        _private: bool,
    ) -> io::Result<ReadStats> {
        super::read_entries_std(path, cancel, out)
    }

    pub fn clone_refcnt(_: &Path) -> Option<u32> {
        None
    }

    pub fn may_share(_: &Path) -> Option<bool> {
        None
    }

    /// Hard links are not identified here (and ReFS can clone blocks), so
    /// nothing is known to be freeable.
    pub fn may_share_blocks(_: &Path) -> bool {
        true
    }

    pub fn allocated(m: &Metadata) -> u64 {
        m.len()
    }
    pub fn device(_: &Metadata) -> u64 {
        0
    }
    pub fn hardlink_id(_: &Metadata) -> Option<(u64, u64)> {
        None
    }
    pub fn file_id(_: &Metadata) -> (u64, u64) {
        (0, 1)
    }

    /// Lossy rendering of a non-UTF-8 (unpaired surrogate) name.
    pub fn lossy_name(raw: &OsStr) -> String {
        raw.to_string_lossy().into_owned()
    }

    /// The raw entry of `base` behind a lossy name: matched against non-UTF-8
    /// entries only (never a UTF-8 sibling that renders the same); `None`
    /// unless exactly one matches.
    pub fn lossy_child(base: &Path, name: &str) -> Option<PathBuf> {
        let mut matches = fs::read_dir(base)
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_str().is_none() && n.to_string_lossy() == *name);
        match (matches.next(), matches.next()) {
            (Some(raw), None) => Some(base.join(raw)),
            _ => None,
        }
    }
}

/// One directory entry with the metadata the scan needs, from `lstat` or,
/// on macOS, from `getattrlistbulk`.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct Entry {
    /// Raw name: paths must be built from it (names need not be UTF-8).
    pub name: OsString,
    pub dir: bool,
    /// Allocated bytes (`st_blocks * 512` on unix).
    pub alloc: u64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// APFS full-clone family (clone id), when this file has identical
    /// clones (`EF_SHARES_ALL_BLOCKS`).
    pub clone: Option<u64>,
    /// Some blocks may be shared with another file (APFS
    /// `EF_MAY_SHARE_BLOCKS`; it stays set once a file was cloned).
    pub may_share: bool,
}

impl Entry {
    fn from_metadata(name: OsString, m: &Metadata) -> Entry {
        let (ino, nlink) = platform::file_id(m);
        Entry {
            name,
            dir: m.is_dir(),
            // Only file links matter; keep dirs comparable across readers.
            nlink: if m.is_dir() { 1 } else { nlink },
            alloc: platform::allocated(m),
            dev: platform::device(m),
            ino,
            clone: None,
            may_share: false,
        }
    }

    /// `(dev, ino)` of a non-directory with more than one link.
    pub fn link(&self) -> Option<(u64, u64)> {
        (!self.dir && self.nlink > 1).then_some((self.dev, self.ino))
    }
}

/// What a directory read found besides its entries.
#[derive(Default)]
pub(crate) struct ReadStats {
    /// Entries that could not be read.
    pub errors: u64,
    /// The filesystem reported clone flags (APFS).
    pub clone_ids: bool,
    /// With --clones, files whose clone flags could not be read (another
    /// volume, a reader without them): those count as possibly shared.
    pub clone_gaps: u64,
    /// Private sizes were requested and the volume returned the clone flags
    /// that pick out partial clones (APFS).
    pub private_sizes: bool,
    /// A partial clone's private size was not reported (no such attribute,
    /// no permission, an I/O error).
    pub private_missing: bool,
    /// `(raw name, bytes)`: APFS private sizes of the partial clones read
    /// (`--private`; empty, so not allocated, otherwise).
    pub private: Vec<(OsString, u64)>,
}

/// Reads `path` with `read_dir` + `lstat`, appending to `out`. Stops early
/// once `cancel` is set.
fn read_entries_std(path: &Path, cancel: &AtomicBool, out: &mut Vec<Entry>) -> io::Result<ReadStats> {
    let mut errors = 0;
    for (i, e) in fs::read_dir(path)?.enumerate() {
        // Huge flat directories: notice a cancel without reading them all.
        if i % 256 == 255 && cancel.load(Ordering::Relaxed) {
            break;
        }
        // DirEntry::metadata does not follow symlinks.
        match e.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
            Ok((n, m)) => out.push(Entry::from_metadata(n, &m)),
            Err(_) => errors += 1,
        }
    }
    Ok(ReadStats {
        errors,
        ..ReadStats::default()
    })
}

/// Files in the APFS full-clone family of the file at `path`, where the
/// system can tell (macOS 14+).
pub fn clone_refcnt(path: &Path) -> Option<u32> {
    platform::clone_refcnt(path)
}

/// Files below `root` may share blocks (or links that cannot be told apart,
/// on Windows), so a freeable lower bound needs shared-block detection.
/// Checked once, for the root's filesystem; with
/// `--cross-fs` the scan may enter other filesystems, so on Linux (where
/// only some filesystems share blocks) it is then always assumed.
pub fn may_share_blocks(root: &Path, one_fs: bool) -> bool {
    platform::may_share_blocks(root) || (!one_fs && cfg!(target_os = "linux"))
}

struct Ctx<'a> {
    progress: &'a Progress,
    /// The scanned root, for paths of private sizes.
    root: &'a Path,
    root_dev: u64,
    one_fs: bool,
    /// Detect APFS clones (`--clones`).
    clones: bool,
    seen_inodes: Mutex<FxHashSet<(u64, u64)>>,
    /// A directory never to descend into (dutype's own cache), as (name, full path).
    exclude: Option<(OsString, PathBuf)>,
}

impl Ctx<'_> {
    /// Allocated size, counting each hard-linked inode only once.
    fn alloc_size(&self, alloc: u64, link: Option<(u64, u64)>) -> u64 {
        if let Some(id) = link
            && !self.seen_inodes.lock().unwrap().insert(id)
        {
            return 0;
        }
        alloc
    }
}

/// The on-disk path of `node` inside `base`: a valid UTF-8 name is joined
/// as is, a non-UTF-8 one is decoded by [`platform::lossy_child`].
pub fn resolve_child(base: &Path, node: &Node) -> Option<PathBuf> {
    match node.lossy {
        false => Some(base.join(&*node.name)),
        true => platform::lossy_child(base, &node.name),
    }
}

/// The on-disk path of `node`, found below `base` through `ancestors`
/// (outermost first). No I/O unless a name on the way is lossy.
pub fn resolve_chain(base: &Path, ancestors: &[&Node], node: &Node) -> Option<PathBuf> {
    ancestors
        .iter()
        .chain(std::iter::once(&node))
        .try_fold(base.to_path_buf(), |p, n| resolve_child(&p, n))
}

/// Name for display and classification, and whether it is not valid UTF-8.
/// No copy when it is valid UTF-8; an invalid name gets the platform's
/// rendering ([`platform::lossy_name`]).
fn display_name(n: OsString) -> (Box<str>, bool) {
    match n.into_string() {
        Ok(s) => (s.into(), false),
        Err(n) => (platform::lossy_name(&n).into(), true),
    }
}

/// Add the private sizes read in the directory `path` to the scan's, by
/// path relative to the root with the names the tree uses. A non-UTF-8
/// name on the way renders like some valid name could, so such a size is
/// left out (and the scan's sizes marked incomplete) rather than risk
/// showing it on another file.
fn record_private(path: &Path, found: Vec<(OsString, u64)>, ctx: &Ctx) {
    let rel = path.strip_prefix(ctx.root).unwrap_or(Path::new(""));
    let Some(rel) = rel.to_str().map(PathBuf::from) else {
        ctx.progress.private_missing.store(true, Ordering::Relaxed);
        return;
    };
    let mut lossy = false;
    let found: Vec<_> = found
        .into_iter()
        .filter_map(|(n, b)| match n.to_str() {
            Some(n) => Some((rel.join(n), b)),
            None => {
                lossy = true;
                None
            }
        })
        .collect();
    if lossy {
        ctx.progress.private_missing.store(true, Ordering::Relaxed);
    }
    ctx.progress.found.lock().unwrap().extend(found);
}

fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Scan `root`. dutype's own cache directory is always skipped, so `--cache`
/// writing a snapshot under the scanned tree never changes the totals.
/// `clones` also reads APFS clone identity (`--clones`), which costs time.
pub fn scan(root: &Path, one_fs: bool, clones: bool, progress: &Progress) -> std::io::Result<Node> {
    let cache_dir = crate::cache::dir().and_then(|d| fs::canonicalize(d).ok());
    scan_excluding(root, one_fs, clones, progress, cache_dir.as_deref())
}

pub fn scan_excluding(
    root: &Path,
    one_fs: bool,
    clones: bool,
    progress: &Progress,
    exclude: Option<&Path>,
) -> std::io::Result<Node> {
    scan_root(root, one_fs, clones, progress, exclude, None, None)
}

/// Rescan one directory of an existing tree. `inherited` is the special
/// category of its parent (e.g. node_modules), so files keep the category a
/// full scan would give them. `root_dev` is the device of the original scan's
/// root: with `one_fs`, mounts are judged against it as in a full scan.
///
/// Fails if `path` is no longer a real directory (replaced by a symlink or a
/// file) or, with `one_fs`, now lies on another filesystem (something was
/// mounted on it): only a full scan treats those like the original scan did.
pub fn scan_subtree(
    path: &Path,
    one_fs: bool,
    clones: bool,
    progress: &Progress,
    inherited: Option<Category>,
    root_dev: Option<u64>,
) -> std::io::Result<Node> {
    if !is_real_dir(path) {
        return Err(std::io::Error::other("no longer a directory"));
    }
    if one_fs && root_dev.is_some_and(|dev| device_of(path).ok() != Some(dev)) {
        return Err(std::io::Error::other("now on another filesystem"));
    }
    let cache_dir = crate::cache::dir().and_then(|d| fs::canonicalize(d).ok());
    scan_root(
        path,
        one_fs,
        clones,
        progress,
        cache_dir.as_deref(),
        inherited,
        root_dev,
    )
}

/// Device id of `path` (0 where the platform has none).
pub fn device_of(path: &Path) -> std::io::Result<u64> {
    fs::metadata(path).map(|m| platform::device(&m))
}

/// A directory, not a symlink to one (matching how scans treat entries).
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn scan_root(
    root: &Path,
    one_fs: bool,
    clones: bool,
    progress: &Progress,
    exclude: Option<&Path>,
    inherited: Option<Category>,
    root_dev: Option<u64>,
) -> std::io::Result<Node> {
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        progress,
        root,
        root_dev: root_dev.unwrap_or_else(|| platform::device(&meta)),
        one_fs,
        clones,
        seen_inodes: Mutex::new(FxHashSet::default()),
        exclude: exclude.and_then(|p| Some((p.file_name()?.to_os_string(), p.to_path_buf()))),
    };
    let name: Box<str> = root.display().to_string().into();
    if !meta.is_dir() {
        let shares = clones.then(|| platform::may_share(root)).flatten();
        if clones {
            let flag = match shares.is_some() {
                true => &progress.clone_ids,
                false => &progress.clone_gaps,
            };
            flag.store(true, Ordering::Relaxed);
        }
        let cat = reg().classify_file(&name);
        return Ok(Node {
            name,
            size: ctx.alloc_size(platform::allocated(&meta), platform::hardlink_id(&meta)),
            cat,
            special: false,
            hardlink: platform::hardlink_id(&meta).is_some(),
            lossy: false,
            // Keeps a cloned file (or one of unknown state) from looking
            // freeable.
            clone: clones && shares != Some(false),
            dir: None,
        });
    }
    let parent = root.parent().map(file_name_of).unwrap_or_default();
    let own_size = platform::allocated(&meta);
    let mut node = scan_dir(root, file_name_of(root).into(), &parent, inherited, own_size, &ctx);
    node.name = name;
    if clones {
        crate::account::account(&mut node, Some(root));
    }
    Ok(node)
}

fn scan_dir(
    path: &Path,
    name: Box<str>,
    parent_name: &str,
    inherited: Option<Category>,
    own_size: u64,
    ctx: &Ctx,
) -> Node {
    let reg = reg();
    let mut entries: Vec<Entry> = Vec::new();
    // A cancelled scan stops descending; its partial result is discarded.
    let stats = match ctx.progress.cancel.load(Ordering::Relaxed) {
        true => ReadStats::default(),
        false => platform::read_entries(
            path,
            &ctx.progress.cancel,
            &mut entries,
            ctx.clones,
            ctx.progress.private,
        )
        .unwrap_or(ReadStats {
            errors: 1,
            ..ReadStats::default()
        }),
    };
    if stats.clone_ids {
        ctx.progress.clone_ids.store(true, Ordering::Relaxed);
    }
    let mut clone_gaps = stats.clone_gaps;
    if clone_gaps > 0 {
        ctx.progress.clone_gaps.store(true, Ordering::Relaxed);
    }
    if stats.private_sizes {
        ctx.progress.private_sizes.store(true, Ordering::Relaxed);
    }
    // A file whose clone flags could not be read may be a partial clone
    // whose private size is then unknown.
    if stats.private_missing || (ctx.progress.private && stats.clone_gaps > 0) {
        ctx.progress.private_missing.store(true, Ordering::Relaxed);
    }
    if !stats.private.is_empty() {
        record_private(path, stats.private, ctx);
    }
    let mut errors = stats.errors;
    if errors > 0 {
        ctx.progress.errors.fetch_add(errors, Ordering::Relaxed);
    }

    // Inside a special dir everything inherits it, unless that category is weak
    // (e.g. ~/.cache) and a more specific one (e.g. huggingface/) is found.
    let own = match inherited {
        Some(c) if !c.is_weak() => None,
        // Rule names are UTF-8, so a non-UTF-8 entry can never match one.
        _ => reg.special_dir(&name, parent_name, entries.iter().filter_map(|e| e.name.to_str())),
    };
    let effective = own.or(inherited);

    let (dirs, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|e| e.dir);

    let mut children: Vec<Node> = dirs
        .into_par_iter()
        .filter(|e| !ctx.one_fs || e.dev == ctx.root_dev)
        // Compare the name first so the full path is only built for a likely match.
        .filter(|e| {
            ctx.exclude
                .as_ref()
                .is_none_or(|(name, full)| e.name != *name || path.join(&e.name) != *full)
        })
        .map(|e| {
            let child_path = path.join(&e.name);
            let (display, lossy) = display_name(e.name);
            let mut node = scan_dir(&child_path, display, &name, effective, e.alloc, ctx);
            node.lossy = lossy;
            node
        })
        .collect();

    // Progress is updated once per directory to keep atomics off the hot path.
    let (file_entries, mut file_bytes) = (files.len() as u64, 0);
    // One allocation for the files instead of growing by doubling: the
    // spare capacity would stay allocated for the life of the tree.
    children.reserve_exact(files.len());
    let mut shared = Vec::new();
    let mut free_base = own_size;
    for (i, e) in files.into_iter().enumerate() {
        if i % 256 == 255 && ctx.progress.cancel.load(Ordering::Relaxed) {
            break;
        }
        let link = e.link();
        let size = ctx.alloc_size(e.alloc, link);
        file_bytes += size;
        // Groups are only accounted with --clones; without it a hard link
        // simply never counts as freeable.
        if ctx.clones && (link.is_some() || e.clone.is_some()) {
            let (key, is_clone, refcnt) = match e.clone {
                Some(id) => ((e.dev, id), true, 0),
                // A hard link that may share blocks (a partial clone, or
                // unknown): 0, so its group is never complete.
                None => ((e.dev, e.ino), false, if e.may_share { 0 } else { 1 }),
            };
            shared.push(Shared {
                idx: children.len() as u32,
                key,
                is_clone,
                ino: e.ino,
                nlink: e.nlink,
                refcnt,
                dup: false,
            });
        }
        let clone = e.clone.is_some() || e.may_share;
        if link.is_none() && !clone {
            free_base += size;
        }
        let (name, lossy) = display_name(e.name);
        let cat = effective.unwrap_or_else(|| reg.classify_file(&name));
        children.push(Node {
            name,
            lossy,
            size,
            cat,
            special: false,
            hardlink: link.is_some(),
            clone,
            dir: None,
        });
    }
    ctx.progress.files.fetch_add(file_entries, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(file_bytes, Ordering::Relaxed);

    let mut cats = vec![0u64; reg.len()].into_boxed_slice();
    cats[effective.unwrap_or(reg.other()).index()] += own_size;
    let mut size = own_size;
    let mut file_count = 0;
    let (mut hardlink, mut clone) = (false, false);
    for c in &children {
        size += c.size;
        file_count += c.files();
        hardlink |= c.hardlink;
        clone |= c.clone;
        match &c.dir {
            Some(d) => {
                cats.iter_mut().zip(d.cats.iter()).for_each(|(a, b)| *a += b);
                errors += d.errors;
                clone_gaps += d.clone_gaps();
                free_base += d.free_base;
            }
            None => cats[c.cat.index()] += c.size,
        }
    }
    let mut dir = DirInfo {
        cats,
        files: file_count,
        errors,
        children,
        free_base,
        clones: (!shared.is_empty() || clone_gaps > 0).then(|| {
            Box::new(CloneInfo {
                shared: shared.into_boxed_slice(),
                gaps: clone_gaps,
                ..CloneInfo::default()
            })
        }),
    };
    dir.sort_children(false);

    Node {
        name,
        size,
        cat: effective.unwrap_or(reg.other()),
        special: own.is_some(),
        hardlink,
        lossy: false,
        clone,
        dir: Some(Box::new(dir)),
    }
}

/// Like [`walk`], but `f` also gets the visited node's deduplicated size
/// (see [`DirInfo::child_dedup`]) and the nodes between `node` and it
/// (outermost first), e.g. to resolve real paths.
pub fn walk_with_ancestors<'a>(
    node: &'a Node,
    rel: &mut PathBuf,
    ancestors: &mut Vec<&'a Node>,
    f: &mut impl FnMut(&'a Node, u64, &Path, &[&'a Node]) -> bool,
) {
    let Some(d) = node.dir.as_deref() else { return };
    for (i, c) in d.children.iter().enumerate() {
        rel.push(&*c.name);
        if f(c, d.child_dedup(i), rel, ancestors) && c.is_dir() {
            ancestors.push(c);
            walk_with_ancestors(c, rel, ancestors, f);
            ancestors.pop();
        }
        rel.pop();
    }
}

/// Depth-first walk below `node`; `f` returns false to skip descending into a dir.
pub fn walk(node: &Node, rel: &mut PathBuf, f: &mut impl FnMut(&Node, &Path) -> bool) {
    for c in node.children() {
        rel.push(&*c.name);
        if f(c, rel) && c.is_dir() {
            walk(c, rel, f);
        }
        rel.pop();
    }
}

/// Keeps the `n` largest items seen (items are ordered by their first field).
pub struct TopN<T: Ord> {
    n: usize,
    heap: BinaryHeap<Reverse<T>>,
}

impl<T: Ord> TopN<T> {
    pub fn new(n: usize) -> Self {
        TopN {
            n,
            heap: BinaryHeap::with_capacity(n + 1),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.heap.len() < self.n {
            self.heap.push(Reverse(item));
        } else if self.heap.peek().is_some_and(|Reverse(min)| item > *min) {
            self.heap.pop();
            self.heap.push(Reverse(item));
        }
    }

    /// Whether an item whose first field compares like `key` would be kept;
    /// lets callers skip building expensive items (paths) that would be dropped.
    pub fn accepts(&self, beats: impl FnOnce(&T) -> bool) -> bool {
        self.heap.len() < self.n || self.heap.peek().is_some_and(|Reverse(min)| beats(min))
    }

    /// Largest first.
    pub fn into_sorted(self) -> Vec<T> {
        let mut v: Vec<T> = self.heap.into_iter().map(|Reverse(t)| t).collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }
}

/// A private size found under a non-UTF-8 name is left out: its rendering
/// could equal a valid sibling's name.
#[cfg(all(test, unix))]
mod record_private_tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn lossy_names_are_left_out() {
        let progress = Progress::default();
        let root = Path::new("/r");
        let ctx = Ctx {
            progress: &progress,
            root,
            root_dev: 0,
            one_fs: true,
            clones: true,
            seen_inodes: Mutex::new(FxHashSet::default()),
            exclude: None,
        };
        let raw = |b: &[u8]| OsStr::from_bytes(b).to_os_string();
        record_private(&root.join("d"), vec![(raw(b"\\xff"), 1)], &ctx);
        assert!(!progress.private_missing.load(Ordering::Relaxed));
        record_private(&root.join("d"), vec![(raw(b"\xff"), 2)], &ctx);
        assert!(progress.private_missing.load(Ordering::Relaxed));
        record_private(&root.join(OsStr::from_bytes(b"\xfe")), vec![(raw(b"f"), 3)], &ctx);
        let got = progress.take_private();
        assert_eq!(got.0.len(), 1);
        let key: PathBuf = ["d", "\\xff"].iter().collect();
        assert_eq!(got.get(&key), Some(1));
    }
}

#[cfg(all(test, unix))]
mod escape_tests {
    use super::platform::{escape, unescape};

    #[test]
    fn round_trips_and_stays_distinct() {
        for raw in [&b"d\xffir"[..], b"d\xfeir", b"a\\b\xff", b"\\x41", b"ok", b"\xc3"] {
            assert_eq!(unescape(&escape(raw)).unwrap(), raw, "{raw:?}");
        }
        assert_ne!(escape(b"d\xffir"), escape(b"d\xfeir"));
        assert_eq!(escape(b"d\xffir"), "d\\xffir");
        assert!(unescape("bad\\q").is_none());
    }
}

#[cfg(test)]
mod size_tests {
    use super::{DirInfo, Node};

    /// One node per file and one `DirInfo` per dir: their sizes are most of
    /// a scan's memory. Clone state lives behind one pointer (`--clones`).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn tree_nodes_stay_small() {
        assert_eq!(std::mem::size_of::<Node>(), 40);
        assert_eq!(std::mem::size_of::<DirInfo>(), 72);
    }
}

#[cfg(test)]
mod share_tests {
    use super::may_share_blocks;

    /// Files from a reader without clone flags (with --clones on macOS) may
    /// share blocks; dirs are left alone.
    #[cfg(unix)]
    #[test]
    fn unknown_clone_state_marks_files() {
        use super::{Entry, platform};
        let entry = |name: &str, dir: bool| Entry {
            name: name.into(),
            dir,
            alloc: 4096,
            dev: 1,
            ino: 2,
            nlink: 1,
            clone: None,
            may_share: false,
        };
        let mut v = vec![entry("f", false), entry("d", true)];
        assert_eq!(platform::unknown_clone_state(&mut v), 1);
        assert_eq!((v[0].may_share, v[1].may_share), (true, false));
    }

    /// APFS always may share blocks; with --cross-fs a Linux scan may enter a
    /// reflink filesystem, so it is assumed there too.
    #[test]
    fn shared_blocks_are_assumed_where_they_can_happen() {
        let tmp = std::env::temp_dir();
        if cfg!(target_os = "macos") {
            assert!(may_share_blocks(&tmp, true));
        }
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(may_share_blocks(&tmp, false));
        }
        if cfg!(windows) {
            assert!(may_share_blocks(&tmp, true));
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod bulk_tests {
    use super::*;
    use std::process::Command;

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dutype-bulk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub/deeper")).unwrap();
        fs::write(d.join("small.txt"), b"hi").unwrap();
        fs::write(d.join("sub/big.bin"), vec![7u8; 3 << 20]).unwrap();
        fs::write(d.join("sub/deeper/empty"), b"").unwrap();
        fs::hard_link(d.join("sub/big.bin"), d.join("link.bin")).unwrap();
        std::os::unix::fs::symlink("sub", d.join("to-sub")).unwrap();
        // A clone: its own inode, full st_blocks (counted again, like du).
        assert!(
            Command::new("cp")
                .arg("-c")
                .arg(d.join("sub/big.bin"))
                .arg(d.join("clone.bin"))
                .status()
                .unwrap()
                .success()
        );
        for i in 0..600 {
            fs::write(d.join(format!("sub/deeper/f{i}")), vec![1u8; i * 100]).unwrap();
        }
        d
    }

    fn sorted(mut v: Vec<Entry>) -> Vec<Entry> {
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    #[test]
    fn bulk_matches_lstat() {
        let d = tree("lstat");
        let cancel = AtomicBool::new(false);
        for dir in [d.clone(), d.join("sub"), d.join("sub/deeper")] {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            assert_eq!(read_entries_std(&dir, &cancel, &mut a).unwrap().errors, 0);
            assert_eq!(
                platform::bulk::read_entries(&dir, &cancel, &mut b, true, true)
                    .unwrap()
                    .unwrap()
                    .errors,
                0
            );
            // lstat knows nothing about clones.
            for e in &mut b {
                (e.clone, e.may_share) = (None, false);
            }
            assert_eq!(sorted(a), sorted(b), "{}", dir.display());
        }
        fs::remove_dir_all(&d).unwrap();
    }

    /// Without --clones the bulk request is the plain one: no clone data,
    /// no clone accounting.
    #[test]
    fn clones_off_reads_no_clone_data() {
        let d = tree("off");
        let cancel = AtomicBool::new(false);
        let mut v = Vec::new();
        let stats = platform::bulk::read_entries(&d, &cancel, &mut v, false, false)
            .unwrap()
            .unwrap();
        assert!(!stats.clone_ids);
        assert!(v.iter().all(|e| e.clone.is_none() && !e.may_share));
        let progress = Progress::default();
        let node = scan_excluding(&d, true, false, &progress, None).unwrap();
        assert!(!progress.clone_ids.load(Ordering::Relaxed));
        assert_eq!(node.dedup(), node.size);
        fs::remove_dir_all(&d).unwrap();
    }

    /// --clones on APFS with only dirs: detection works (dirs report the
    /// flags), and a tree without files has known freeable bytes anyway.
    #[test]
    fn dirs_only_tree_reports_detection() {
        let d = std::env::temp_dir().join(format!("dutype-bulk-dirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/b")).unwrap();
        let progress = Progress::default();
        let node = scan_excluding(&d, true, true, &progress, None).unwrap();
        assert!(progress.clone_ids.load(Ordering::Relaxed));
        assert_eq!(crate::report::freeable_of(&node, false), Some(node.freeable()));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn total_matches_du() {
        let d = tree("du");
        let node = scan_excluding(&d, true, true, &Progress::default(), None).unwrap();
        let out = Command::new("/usr/bin/du").arg("-skx").arg(&d).output().unwrap();
        let kib: u64 = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(node.size, kib * 1024);
        fs::remove_dir_all(&d).unwrap();
    }

    /// The issue #5 fixture: an 8 MiB file, three clones, 1 MiB of one of
    /// them overwritten, and a hard link.
    #[test]
    fn clone_families_count_once() {
        use std::io::{Seek, Write};
        let d = std::env::temp_dir().join(format!("dutype-bulk-clones-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a"), vec![7u8; 8 << 20]).unwrap();
        for c in ["c1", "c2", "c3"] {
            assert!(
                Command::new("cp")
                    .arg("-c")
                    .arg(d.join("a"))
                    .arg(d.join(c))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut f = fs::OpenOptions::new().write(true).open(d.join("c3")).unwrap();
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        f.write_all(&vec![9u8; 1 << 20]).unwrap();
        f.sync_all().unwrap();
        fs::hard_link(d.join("a"), d.join("sub/hl")).unwrap();

        let progress = Progress::default();
        let node = scan_excluding(&d, true, true, &progress, None).unwrap();
        assert!(progress.clone_ids.load(Ordering::Relaxed), "APFS on macOS 14+ expected");
        let out = Command::new("/usr/bin/du").arg("-skx").arg(&d).output().unwrap();
        let kib: u64 = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(node.size, kib * 1024);
        assert_eq!(node.size, 32 << 20);
        // a/c1/c2 (one family) once, c3 (a partial clone now) in full.
        assert_eq!(node.dedup(), 16 << 20);
        // The family, with both links of `a`, is all inside; c3 may share.
        assert_eq!(node.freeable(), 8 << 20);
        let sub = node.children().iter().find(|c| &*c.name == "sub").unwrap();
        assert_eq!(sub.freeable(), 0);

        // Parallel scans pick the same representative every time.
        let dedups = |n: &Node| {
            (0..n.children().len())
                .map(|i| n.dir.as_ref().unwrap().child_dedup(i))
                .collect::<Vec<_>>()
        };
        let first = dedups(&node);
        for _ in 0..5 {
            assert_eq!(
                dedups(&scan_excluding(&d, true, true, &Progress::default(), None).unwrap()),
                first
            );
        }
        // A clone in `sub`, scanned alone: the rest of its family is
        // outside, so it frees nothing for sure and has no copy to drop.
        assert!(
            Command::new("cp")
                .arg("-c")
                .arg(d.join("a"))
                .arg(d.join("sub/c4"))
                .status()
                .unwrap()
                .success()
        );
        let sub = scan_excluding(&d.join("sub"), true, true, &Progress::default(), None).unwrap();
        assert_eq!((sub.size, sub.dedup(), sub.freeable()), (16 << 20, 8 << 20, 0));

        // A single file as the root: a clone (full or partial) frees nothing
        // for sure, a plain file all of it.
        fs::write(d.join("plain"), vec![1u8; 1 << 20]).unwrap();
        let file = |n: &str| scan_excluding(&d.join(n), true, true, &Progress::default(), None).unwrap();
        assert_eq!(file("c1").freeable(), 0);
        assert_eq!(file("c3").freeable(), 0);
        assert_eq!(file("plain").freeable(), 1 << 20);
        // ... and its clone flags count as detection.
        let progress = Progress::default();
        scan_excluding(&d.join("plain"), true, true, &progress, None).unwrap();
        assert!(progress.clone_ids.load(Ordering::Relaxed));
        assert!(!progress.clone_gaps.load(Ordering::Relaxed));
        fs::remove_dir_all(&d).unwrap();
    }

    /// `--private`: a clone with 1 MiB overwritten holds 1 MiB alone. New
    /// files in a temp dir are younger than any local snapshot, which would
    /// otherwise hold their blocks (and make the private size 0).
    #[test]
    fn private_size_of_partial_clone() {
        use std::io::{Seek, Write};
        let since = test_support::stamp();
        let d = std::env::temp_dir().join(format!("dutype-bulk-private-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a"), vec![7u8; 8 << 20]).unwrap();
        fs::write(d.join("plain"), vec![1u8; 1 << 20]).unwrap();
        for c in ["full", "partial", "linked", "sub/deep"] {
            let ok = Command::new("cp").arg("-c").arg(d.join("a")).arg(d.join(c)).status();
            assert!(ok.unwrap().success());
        }
        for c in ["partial", "linked", "sub/deep"] {
            let mut f = fs::OpenOptions::new().write(true).open(d.join(c)).unwrap();
            f.seek(std::io::SeekFrom::Start(2 << 20)).unwrap();
            f.write_all(&vec![9u8; 1 << 20]).unwrap();
            f.sync_all().unwrap();
        }
        // A hard-linked partial clone frees nothing while the other link stays.
        fs::hard_link(d.join("linked"), d.join("linked2")).unwrap();

        let progress = Progress {
            private: true,
            ..Progress::default()
        };
        let mut node = scan_excluding(&d, true, true, &progress, None).unwrap();
        assert!(progress.private_sizes.load(Ordering::Relaxed) && !progress.private_missing.load(Ordering::Relaxed));
        let table = progress.take_private();
        // Which files get a size does not depend on snapshots; the values
        // do (see test_support). Keys are paths below the root.
        let mut keys: Vec<_> = table.0.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, [PathBuf::from("partial"), Path::new("sub").join("deep")]);
        let size = table.get(Path::new("partial"));
        test_support::assert_private(size, 1 << 20, &since);
        test_support::assert_private(table.get(&Path::new("sub").join("deep")), 1 << 20, &since);
        // Sorting the tree does not touch it.
        node.dir.as_mut().unwrap().sort_children(true);

        // In the JSON: on those files only, omitted elsewhere.
        let meta = crate::report::Meta {
            elapsed: std::time::Duration::ZERO,
            cached_at: None,
            unreadable: 0,
            clones: true,
            clone_ids: true,
            clone_gaps: false,
            shares: true,
            size_mode: crate::report::SizeMode::Du,
            private: Some(&table),
        };
        let r = serde_json::to_value(crate::report::build(&node, &d, &meta, 20)).unwrap();
        assert_eq!(r["accounting"]["private"], true);
        for list in ["children", "largest_files"] {
            for e in r[list].as_array().unwrap() {
                let path = e["path"].as_str().unwrap();
                let rel = Path::new(path).strip_prefix(&d).unwrap();
                let want = table.get(rel);
                assert_eq!(e.get("private_bytes").and_then(|v| v.as_u64()), want, "{list} {path}");
            }
        }
        let other = crate::category::reg().other();
        let found = crate::report::find(&node, &d, other, 20, meta.size_mode, true, meta.private);
        let partial = found.iter().find(|e| e.path.ends_with("/partial")).unwrap();
        assert_eq!(partial.private_bytes, size);
        let plain_meta = crate::report::Meta { private: None, ..meta };
        let r = serde_json::to_value(crate::report::build(&node, &d, &plain_meta, 20)).unwrap();
        assert_eq!(r["accounting"]["private"], false);
        assert!(
            r["largest_files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e.get("private_bytes").is_none())
        );

        // A rescan of `sub` replaces only what is below it.
        let mut merged = table.clone();
        let mut fresh = PrivateSizes::default();
        fresh.0.insert(PathBuf::from("new"), 7);
        merged.replace_below(Path::new("sub"), fresh);
        assert_eq!(merged.get(Path::new("partial")), size);
        assert_eq!(merged.get(&Path::new("sub").join("deep")), None);
        assert_eq!(merged.get(&Path::new("sub").join("new")), Some(7));

        // Not requested: nothing read. Folder values never include it.
        let progress = Progress::default();
        let plain = scan_excluding(&d, true, true, &progress, None).unwrap();
        assert!(!progress.private_sizes.load(Ordering::Relaxed));
        assert!(progress.take_private().0.is_empty());
        // Without clone flags partial clones cannot be told apart.
        let progress = Progress {
            private: true,
            ..Progress::default()
        };
        scan_excluding(&d, true, false, &progress, None).unwrap();
        assert!(!progress.private_sizes.load(Ordering::Relaxed));
        assert!(progress.take_private().0.is_empty());
        assert_eq!(
            (node.size, node.dedup(), node.freeable()),
            (plain.size, plain.dedup(), plain.freeable())
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn missing_dir_is_an_error() {
        let mut v = Vec::new();
        let r = platform::bulk::read_entries(
            Path::new("/nonexistent-dutype"),
            &AtomicBool::new(false),
            &mut v,
            true,
            false,
        );
        assert!(r.unwrap().is_err());
    }
}

/// `--private` is accepted where private sizes do not exist: nothing is
/// read and the scan says so.
#[cfg(all(test, not(target_os = "macos")))]
mod private_tests {
    use super::*;

    #[test]
    fn private_is_a_no_op_elsewhere() {
        let d = std::env::temp_dir().join(format!("dutype-private-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/f"), vec![1u8; 1 << 16]).unwrap();
        let progress = Progress {
            private: true,
            ..Progress::default()
        };
        scan_excluding(&d, true, true, &progress, None).unwrap();
        assert!(!progress.private_sizes.load(Ordering::Relaxed));
        assert!(progress.take_private().0.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }
}

/// Checks of APFS private sizes that tolerate a local snapshot taken during
/// the test (it holds the file's blocks, so its private size becomes 0).
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::process::Command;

    /// Local time in `tmutil listlocalsnapshotdates` format, taken before
    /// a test creates its files.
    pub fn stamp() -> String {
        let out = Command::new("date").arg("+%Y-%m-%d-%H%M%S").output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A local snapshot of the data volume was taken at or after `since`.
    fn snapshot_since(since: &str) -> Option<String> {
        ["/System/Volumes/Data", "/"].iter().find_map(|vol| {
            let out = Command::new("tmutil")
                .args(["listlocalsnapshotdates", vol])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| l.len() == since.len() && l.as_bytes()[0].is_ascii_digit())
                .find(|l| *l >= since)
                .map(str::to_string)
        })
    }

    /// `got` must be read; it must equal `want` unless a snapshot taken
    /// since `since` explains a 0.
    pub fn assert_private(got: Option<u64>, want: u64, since: &str) {
        let got = got.expect("private size of a partial clone");
        if got == want {
            return;
        }
        match snapshot_since(since) {
            Some(snap) if got == 0 => {
                eprintln!("private size is 0: local snapshot {snap} taken during the test; not checking {want}")
            }
            _ => panic!("private size {got}, expected {want}"),
        }
    }
}
