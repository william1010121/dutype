use crate::category::{Category, reg};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
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
    use std::fs::Metadata;
    use std::os::unix::fs::MetadataExt;

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
}

#[cfg(not(unix))]
mod platform {
    use std::fs::Metadata;

    pub fn allocated(m: &Metadata) -> u64 {
        m.len()
    }
    pub fn device(_: &Metadata) -> u64 {
        0
    }
    pub fn hardlink_id(_: &Metadata) -> Option<(u64, u64)> {
        None
    }
}

struct Ctx<'a> {
    progress: &'a Progress,
    root_dev: u64,
    one_fs: bool,
    seen_inodes: Mutex<FxHashSet<(u64, u64)>>,
    /// A directory never to descend into (dutype's own cache), as (name, full path).
    exclude: Option<(String, PathBuf)>,
    /// A file with more than one link was seen.
    saw_hardlink: AtomicBool,
}

impl Ctx<'_> {
    fn error(&self) {
        self.progress.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Allocated size, counting each hard-linked inode only once.
    fn alloc_size(&self, m: &Metadata) -> u64 {
        if let Some(id) = platform::hardlink_id(m) {
            self.saw_hardlink.store(true, Ordering::Relaxed);
            if !self.seen_inodes.lock().unwrap().insert(id) {
                return 0;
            }
        }
        platform::allocated(m)
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
    scan_root(root, one_fs, progress, exclude, None).map(|(node, _)| node)
}

/// Result of rescanning one directory of an existing tree.
pub struct Subtree {
    pub node: Node,
    /// The subtree has hard-linked files. Their blocks may already be counted
    /// elsewhere in the tree, so splicing could double count: rescan all.
    pub has_hardlinks: bool,
}

/// Rescan one directory of an existing tree. `inherited` is the special
/// category of its parent (e.g. node_modules), so files keep the category a
/// full scan would give them.
pub fn scan_subtree(
    path: &Path,
    one_fs: bool,
    progress: &Progress,
    inherited: Option<Category>,
) -> std::io::Result<Subtree> {
    let cache_dir = crate::cache::dir().and_then(|d| fs::canonicalize(d).ok());
    let (node, has_hardlinks) = scan_root(path, one_fs, progress, cache_dir.as_deref(), inherited)?;
    Ok(Subtree { node, has_hardlinks })
}

fn scan_root(
    root: &Path,
    one_fs: bool,
    progress: &Progress,
    exclude: Option<&Path>,
    inherited: Option<Category>,
) -> std::io::Result<(Node, bool)> {
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        progress,
        root_dev: platform::device(&meta),
        one_fs,
        seen_inodes: Mutex::new(FxHashSet::default()),
        exclude: exclude.and_then(|p| Some((p.file_name()?.to_string_lossy().into_owned(), p.to_path_buf()))),
        saw_hardlink: AtomicBool::new(false),
    };
    let name: Box<str> = root.display().to_string().into();
    if !meta.is_dir() {
        let cat = reg().classify_file(&name);
        let node = Node {
            name,
            size: ctx.alloc_size(&meta),
            cat,
            special: false,
            dir: None,
        };
        return Ok((node, ctx.saw_hardlink.into_inner()));
    }
    let parent = root.parent().map(file_name_of).unwrap_or_default();
    let mut node = scan_dir(root, file_name_of(root).into(), &parent, inherited, &meta, &ctx);
    node.name = name;
    Ok((node, ctx.saw_hardlink.into_inner()))
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
    let mut entries: Vec<(String, Metadata)> = Vec::new();
    let mut errors = 0u64;
    // A cancelled scan stops descending; its partial result is discarded.
    let read = match ctx.progress.cancel.load(Ordering::Relaxed) {
        true => Ok(None),
        false => fs::read_dir(path).map(Some),
    };
    match read {
        Ok(Some(rd)) => {
            for e in rd {
                // DirEntry::metadata does not follow symlinks.
                match e.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
                    Ok((n, m)) => entries.push((n.to_string_lossy().into_owned(), m)),
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
        _ => reg.special_dir(&name, parent_name, entries.iter().map(|(n, _)| n.as_str())),
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
            scan_dir(&child_path, n.into(), &name, effective, &m, ctx)
        })
        .collect();

    // Progress is updated once per directory to keep atomics off the hot path.
    let (file_entries, mut file_bytes) = (files.len() as u64, 0);
    for (n, m) in files {
        let size = ctx.alloc_size(&m);
        file_bytes += size;
        let cat = effective.unwrap_or_else(|| reg.classify_file(&n));
        children.push(Node {
            name: n.into(),
            size,
            cat,
            special: false,
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
    for c in &children {
        size += c.size;
        file_count += c.files();
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
        dir: Some(Box::new(DirInfo {
            cats,
            files: file_count,
            errors,
            children,
        })),
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
