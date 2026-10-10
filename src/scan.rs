use crate::category::{self, Category, COUNT};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
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
    pub cats: [u64; COUNT],
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

    pub fn cats(&self) -> [u64; COUNT] {
        match &self.dir {
            Some(d) => d.cats,
            None => {
                let mut c = [0; COUNT];
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

struct Ctx<'a> {
    progress: &'a Progress,
    root_dev: u64,
    one_fs: bool,
    seen_inodes: Mutex<HashSet<(u64, u64)>>,
}

impl Ctx<'_> {
    fn error(&self) {
        self.progress.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Allocated size, counting each hard-linked inode only once.
    fn alloc_size(&self, m: &Metadata) -> u64 {
        if m.nlink() > 1 && !m.is_dir() {
            let mut seen = self.seen_inodes.lock().unwrap();
            if !seen.insert((m.dev(), m.ino())) {
                return 0;
            }
        }
        m.blocks() * 512
    }
}

pub fn scan(root: &Path, one_fs: bool, progress: &Progress) -> std::io::Result<Node> {
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        progress,
        root_dev: meta.dev(),
        one_fs,
        seen_inodes: Mutex::new(HashSet::new()),
    };
    let name: Box<str> = root.display().to_string().into();
    if !meta.is_dir() {
        let cat = category::from_name(&name);
        return Ok(Node { name, size: ctx.alloc_size(&meta), cat, special: false, dir: None });
    }
    Ok(scan_dir(root, name, None, &meta, &ctx))
}

fn scan_dir(path: &Path, name: Box<str>, inherited: Option<Category>, meta: &Metadata, ctx: &Ctx) -> Node {
    let mut entries: Vec<(String, Metadata)> = Vec::new();
    match fs::read_dir(path) {
        Ok(rd) => {
            for e in rd {
                // DirEntry::metadata does not follow symlinks on unix.
                match e.and_then(|e| Ok((e.file_name(), e.metadata()?))) {
                    Ok((n, m)) => entries.push((n.to_string_lossy().into_owned(), m)),
                    Err(_) => ctx.error(),
                }
            }
        }
        Err(_) => ctx.error(),
    }

    let own = match inherited {
        Some(_) => None,
        None => {
            let dir_name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            category::special_dir(&dir_name, entries.iter().map(|(n, _)| n.as_str()))
        }
    };
    let effective = inherited.or(own);

    let (dirs, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(_, m)| m.is_dir());

    let mut children: Vec<Node> = dirs
        .into_par_iter()
        .filter(|(_, m)| !ctx.one_fs || m.dev() == ctx.root_dev)
        .map(|(n, m)| {
            let child_path = path.join(&n);
            scan_dir(&child_path, n.into(), effective, &m, ctx)
        })
        .collect();

    // Progress is updated once per directory to keep atomics off the hot path.
    let (file_entries, mut file_bytes) = (files.len() as u64, 0);
    for (n, m) in files {
        let size = ctx.alloc_size(&m);
        file_bytes += size;
        let cat = effective.unwrap_or_else(|| category::from_name(&n));
        children.push(Node { name: n.into(), size, cat, special: false, dir: None });
    }
    ctx.progress.files.fetch_add(file_entries, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(file_bytes, Ordering::Relaxed);

    let own_size = ctx.alloc_size(meta);
    let mut cats = [0u64; COUNT];
    cats[effective.unwrap_or(Category::Other).index()] += own_size;
    let mut size = own_size;
    let mut file_count = 0;
    for c in &children {
        size += c.size;
        file_count += c.files();
        match &c.dir {
            Some(d) => cats.iter_mut().zip(d.cats).for_each(|(a, b)| *a += b),
            None => cats[c.cat.index()] += c.size,
        }
    }
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    Node {
        name,
        size,
        cat: effective.unwrap_or(Category::Other),
        special: own.is_some(),
        dir: Some(Box::new(DirInfo { cats, files: file_count, children })),
    }
}
