use crate::category::{Category, reg};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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
    pub children: Vec<Node>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.dir.is_some()
    }

    pub fn children(&self) -> &[Node] {
        self.dir.as_ref().map_or(&[], |d| d.children.as_slice())
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

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn scan(root: &Path, one_fs: bool, progress: &Progress) -> std::io::Result<Node> {
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        progress,
        root_dev: platform::device(&meta),
        one_fs,
        seen_inodes: Mutex::new(FxHashSet::default()),
    };
    let name: Box<str> = root.display().to_string().into();
    if !meta.is_dir() {
        let cat = reg().classify_file(&name);
        return Ok(Node { name, size: ctx.alloc_size(&meta), cat, special: false, dir: None });
    }
    let parent = root.parent().map(file_name_of).unwrap_or_default();
    let mut node = scan_dir(root, file_name_of(root).into(), &parent, None, &meta, &ctx);
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
    let mut entries: Vec<(String, Metadata)> = Vec::new();
    match fs::read_dir(path) {
        Ok(rd) => {
            for e in rd {
                // DirEntry::metadata does not follow symlinks.
                match e.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
                    Ok((n, m)) => entries.push((n.to_string_lossy().into_owned(), m)),
                    Err(_) => ctx.error(),
                }
            }
        }
        Err(_) => ctx.error(),
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
        children.push(Node { name: n.into(), size, cat, special: false, dir: None });
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
            Some(d) => cats.iter_mut().zip(d.cats.iter()).for_each(|(a, b)| *a += b),
            None => cats[c.cat.index()] += c.size,
        }
    }
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    Node {
        name,
        size,
        cat: effective.unwrap_or(reg.other()),
        special: own.is_some(),
        dir: Some(Box::new(DirInfo { cats, files: file_count, children })),
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
        TopN { n, heap: BinaryHeap::with_capacity(n + 1) }
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
