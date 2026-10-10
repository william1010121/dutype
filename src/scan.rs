use crate::category::{Category, reg};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
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

    pub fn files(&self) -> u64 {
        self.dir.as_ref().map_or(1, |d| d.files)
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

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    /// Set to stop a running scan early (the result is then incomplete).
    pub cancel: AtomicBool,
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

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
    use std::ffi::OsStr;
    use std::fs::{self, Metadata};
    use std::path::{Path, PathBuf};

    pub fn allocated(m: &Metadata) -> u64 {
        m.len()
    }
    pub fn device(_: &Metadata) -> u64 {
        0
    }
    pub fn hardlink_id(_: &Metadata) -> Option<(u64, u64)> {
        None
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

struct Ctx<'a> {
    progress: &'a Progress,
    root_dev: u64,
    one_fs: bool,
    seen_inodes: Mutex<FxHashSet<(u64, u64)>>,
    /// A directory never to descend into (dutype's own cache), as (name, full path).
    exclude: Option<(OsString, PathBuf)>,
}

impl Ctx<'_> {
    fn error(&self) {
        self.progress.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Allocated size, counting each hard-linked inode only once.
    fn alloc_size(&self, m: &Metadata) -> u64 {
        if let Some(id) = platform::hardlink_id(m)
            && !self.seen_inodes.lock().unwrap().insert(id)
        {
            return 0;
        }
        platform::allocated(m)
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

fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Scan `root`. dutype's own cache directory is always skipped, so `--cache`
/// writing a snapshot under the scanned tree never changes the totals.
pub fn scan(root: &Path, one_fs: bool, progress: &Progress) -> std::io::Result<Node> {
    let cache_dir = crate::cache::dir().and_then(|d| fs::canonicalize(d).ok());
    scan_excluding(root, one_fs, progress, cache_dir.as_deref())
}

pub fn scan_excluding(root: &Path, one_fs: bool, progress: &Progress, exclude: Option<&Path>) -> std::io::Result<Node> {
    scan_root(root, one_fs, progress, exclude, None, None)
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
    scan_root(path, one_fs, progress, cache_dir.as_deref(), inherited, root_dev)
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
    progress: &Progress,
    exclude: Option<&Path>,
    inherited: Option<Category>,
    root_dev: Option<u64>,
) -> std::io::Result<Node> {
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        progress,
        root_dev: root_dev.unwrap_or_else(|| platform::device(&meta)),
        one_fs,
        seen_inodes: Mutex::new(FxHashSet::default()),
        exclude: exclude.and_then(|p| Some((p.file_name()?.to_os_string(), p.to_path_buf()))),
    };
    let name: Box<str> = root.display().to_string().into();
    if !meta.is_dir() {
        let cat = reg().classify_file(&name);
        return Ok(Node {
            name,
            size: ctx.alloc_size(&meta),
            cat,
            special: false,
            hardlink: platform::hardlink_id(&meta).is_some(),
            lossy: false,
            dir: None,
        });
    }
    let parent = root.parent().map(file_name_of).unwrap_or_default();
    let mut node = scan_dir(root, file_name_of(root).into(), &parent, inherited, &meta, &ctx);
    node.name = name;
    Ok(node)
}

fn scan_dir(
    path: &Path,
    name: Box<str>,
    parent_name: &str,
    inherited: Option<Category>,
    meta: &Metadata,
    ctx: &Ctx,
) -> Node {
    let reg = reg();
    // Raw names: paths must be built from them (names need not be UTF-8);
    // only display and classification use a lossy string.
    let mut entries: Vec<(OsString, Metadata)> = Vec::new();
    let mut errors = 0u64;
    // A cancelled scan stops descending; its partial result is discarded.
    let read = match ctx.progress.cancel.load(Ordering::Relaxed) {
        true => Ok(None),
        false => fs::read_dir(path).map(Some),
    };
    match read {
        Ok(Some(rd)) => {
            for (i, e) in rd.enumerate() {
                // Huge flat directories: notice a cancel without reading them all.
                if i % 256 == 255 && ctx.progress.cancel.load(Ordering::Relaxed) {
                    break;
                }
                // DirEntry::metadata does not follow symlinks.
                match e.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
                    Ok((n, m)) => entries.push((n, m)),
                    Err(_) => {
                        ctx.error();
                        errors += 1;
                    }
                }
            }
        }
        Ok(None) => {}
        Err(_) => {
            ctx.error();
            errors += 1;
        }
    }

    // Inside a special dir everything inherits it, unless that category is weak
    // (e.g. ~/.cache) and a more specific one (e.g. huggingface/) is found.
    let own = match inherited {
        Some(c) if !c.is_weak() => None,
        // Rule names are UTF-8, so a non-UTF-8 entry can never match one.
        _ => reg.special_dir(&name, parent_name, entries.iter().filter_map(|(n, _)| n.to_str())),
    };
    let effective = own.or(inherited);

    let (dirs, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(_, m)| m.is_dir());

    let mut children: Vec<Node> = dirs
        .into_par_iter()
        .filter(|(_, m)| !ctx.one_fs || platform::device(m) == ctx.root_dev)
        // Compare the name first so the full path is only built for a likely match.
        .filter(|(n, _)| {
            ctx.exclude
                .as_ref()
                .is_none_or(|(name, full)| n != name || path.join(n) != *full)
        })
        .map(|(n, m)| {
            let child_path = path.join(&n);
            let (display, lossy) = display_name(n);
            let mut node = scan_dir(&child_path, display, &name, effective, &m, ctx);
            node.lossy = lossy;
            node
        })
        .collect();

    // Progress is updated once per directory to keep atomics off the hot path.
    let (file_entries, mut file_bytes) = (files.len() as u64, 0);
    for (i, (n, m)) in files.into_iter().enumerate() {
        if i % 256 == 255 && ctx.progress.cancel.load(Ordering::Relaxed) {
            break;
        }
        let hardlink = platform::hardlink_id(&m).is_some();
        let size = ctx.alloc_size(&m);
        file_bytes += size;
        let (name, lossy) = display_name(n);
        let cat = effective.unwrap_or_else(|| reg.classify_file(&name));
        children.push(Node {
            name,
            lossy,
            size,
            cat,
            special: false,
            hardlink,
            dir: None,
        });
    }
    ctx.progress.files.fetch_add(file_entries, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(file_bytes, Ordering::Relaxed);

    let own_size = ctx.alloc_size(meta);
    let mut cats = vec![0u64; reg.len()].into_boxed_slice();
    cats[effective.unwrap_or(reg.other()).index()] += own_size;
    let mut size = own_size;
    let mut file_count = 0;
    let mut hardlink = false;
    for c in &children {
        size += c.size;
        file_count += c.files();
        hardlink |= c.hardlink;
        match &c.dir {
            Some(d) => {
                cats.iter_mut().zip(d.cats.iter()).for_each(|(a, b)| *a += b);
                errors += d.errors;
            }
            None => cats[c.cat.index()] += c.size,
        }
    }
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    Node {
        name,
        size,
        cat: effective.unwrap_or(reg.other()),
        special: own.is_some(),
        hardlink,
        lossy: false,
        dir: Some(Box::new(DirInfo {
            cats,
            files: file_count,
            errors,
            children,
        })),
    }
}

/// Like [`walk`], but `f` also gets the nodes between `node` and the visited
/// one (outermost first), e.g. to resolve real paths.
pub fn walk_with_ancestors<'a>(
    node: &'a Node,
    rel: &mut PathBuf,
    ancestors: &mut Vec<&'a Node>,
    f: &mut impl FnMut(&'a Node, &Path, &[&'a Node]) -> bool,
) {
    for c in node.children() {
        rel.push(&*c.name);
        if f(c, rel, ancestors) && c.is_dir() {
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
