//! Non-interactive output: a human-readable text report and a JSON document
//! meant for scripts and LLM agents.

use crate::category::{Category, by_size, dominant};
use crate::scan::{Node, TopN};
use crate::ui::human;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Which size lists are ranked by (and the text report shows).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    /// Allocated bytes, like `du -skx` (hard links once).
    #[default]
    Du,
    /// Like `du`, but each APFS clone family counted once.
    Dedup,
}

impl SizeMode {
    fn pick(self, du: u64, dedup: u64) -> u64 {
        match self {
            SizeMode::Du => du,
            SizeMode::Dedup => dedup,
        }
    }
}

/// Scan facts the report shows besides the tree.
pub struct Meta {
    pub elapsed: Duration,
    pub cached_at: Option<SystemTime>,
    pub unreadable: u64,
    pub clone_ids: bool,
    pub size_mode: SizeMode,
}

/// How sizes were counted, so agents know what the numbers mean.
#[derive(Serialize)]
pub struct Accounting {
    /// `size_bytes`: allocated bytes, hard links counted once (`du -skx`).
    pub size: &'static str,
    /// What the lists are ranked by.
    pub size_mode: SizeMode,
    /// `apfs-full`: identical APFS clones are detected (`dedup_bytes`,
    /// `freeable_min_bytes`); `unsupported`: they are counted in full and
    /// `dedup_bytes` equals `size_bytes`.
    pub clone_detection: &'static str,
    pub caveats: Vec<&'static str>,
}

fn accounting(meta: &Meta) -> Accounting {
    Accounting {
        size: "du-allocated",
        size_mode: meta.size_mode,
        clone_detection: if meta.clone_ids { "apfs-full" } else { "unsupported" },
        caveats: if meta.clone_ids {
            vec!["partial-clones-not-deduplicated", "snapshots-not-considered"]
        } else {
            vec!["snapshots-not-considered"]
        },
    }
}

#[derive(Serialize)]
pub struct Report {
    /// Absolute path that was scanned.
    pub path: String,
    pub size_bytes: u64,
    pub size: String,
    /// `size_bytes` with each APFS clone family counted once.
    pub dedup_bytes: u64,
    /// `size_bytes - dedup_bytes`: bytes that are clones of other files.
    pub clone_shared_bytes: u64,
    /// Lower bound of what deleting the whole folder would free.
    pub freeable_min_bytes: u64,
    pub accounting: Accounting,
    pub files: u64,
    pub scan_seconds: f64,
    /// Unix time of the scan when it was read from the `--cache`, else null.
    pub cached_at: Option<u64>,
    /// Entries that could not be read (permissions, races).
    pub unreadable: u64,
    /// Every category with a non-zero size, biggest first.
    pub categories: Vec<CategoryShare>,
    /// Direct children, biggest first.
    pub children: Vec<Child>,
    /// Largest directories matched by a directory rule (node_modules, Python
    /// envs, build output, caches, ...): the usual cleanup candidates.
    pub special_dirs: Vec<Entry>,
    pub largest_files: Vec<Entry>,
}

#[derive(Serialize)]
pub struct CategoryShare {
    pub name: &'static str,
    pub size_bytes: u64,
    pub size: String,
    pub percent: f64,
}

#[derive(Serialize)]
pub struct Child {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub size_bytes: u64,
    pub size: String,
    pub percent: f64,
    pub files: u64,
    /// Set when the child itself is a special dir (e.g. "node_modules").
    pub special: Option<&'static str>,
    /// Up to three biggest categories inside this child.
    pub top_categories: Vec<CategoryShare>,
    pub dedup_bytes: u64,
    pub freeable_min_bytes: u64,
    /// It is, or holds, a file that may share blocks with others (APFS clones).
    pub clone: bool,
}

#[derive(Serialize)]
pub struct Entry {
    pub path: String,
    pub category: &'static str,
    pub size_bytes: u64,
    pub size: String,
    pub dedup_bytes: u64,
    pub freeable_min_bytes: u64,
    pub clone: bool,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn shares(cats: &[u64], total: u64, limit: usize) -> Vec<CategoryShare> {
    by_size(cats)
        .into_iter()
        .take(limit)
        .map(|c| CategoryShare {
            name: c.label(),
            size_bytes: cats[c.index()],
            size: human(cats[c.index()]),
            percent: round1(cats[c.index()] as f64 * 100.0 / total.max(1) as f64),
        })
        .collect()
}

/// A node of a list, with its deduplicated size (which for a file depends
/// on its parent).
struct Item {
    rank: u64,
    rel: PathBuf,
    cat: Category,
    size: u64,
    dedup: u64,
    freeable: u64,
    clone: bool,
}

impl Item {
    fn of(n: &Node, dedup: u64, rel: &Path, mode: SizeMode) -> Item {
        Item {
            rank: mode.pick(n.size, dedup),
            rel: rel.to_path_buf(),
            cat: n.cat,
            size: n.size,
            dedup,
            freeable: n.freeable(),
            clone: n.clone,
        }
    }

    fn entry(self, root: &Path) -> Entry {
        Entry {
            path: root.join(self.rel).display().to_string(),
            category: self.cat.label(),
            size_bytes: self.size,
            size: human(self.size),
            dedup_bytes: self.dedup,
            freeable_min_bytes: self.freeable,
            clone: self.clone,
        }
    }
}

// Ranked like the (size, path) tuples lists used before size modes.
impl PartialEq for Item {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o).is_eq()
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.rank.cmp(&o.rank).then_with(|| self.rel.cmp(&o.rel))
    }
}

/// Depth-first walk below `node` like [`crate::scan::walk`], also passing
/// each node's deduplicated size.
fn walk(node: &Node, rel: &mut PathBuf, f: &mut impl FnMut(&Node, u64, &Path) -> bool) {
    let Some(d) = &node.dir else { return };
    for (i, c) in d.children.iter().enumerate() {
        rel.push(&*c.name);
        if f(c, d.child_dedup(i), rel) && c.is_dir() {
            walk(c, rel, f);
        }
        rel.pop();
    }
}

/// Unix seconds, for the `cached_at` output field.
pub fn unix_secs(t: Option<SystemTime>) -> Option<u64> {
    t.map(|t| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

/// Indices of `root`'s children, biggest first by `mode`.
pub fn ranked_children(root: &Node, mode: SizeMode) -> Vec<usize> {
    let mut order: Vec<usize> = (0..root.children().len()).collect();
    if let (SizeMode::Dedup, Some(d)) = (mode, &root.dir) {
        // Like the TUI's dedup order: ties by name.
        order.sort_by(|&a, &b| {
            (d.child_dedup(b).cmp(&d.child_dedup(a))).then_with(|| d.children[a].name.cmp(&d.children[b].name))
        });
    }
    order
}

pub fn build(root: &Node, root_path: &Path, meta: &Meta, top: usize) -> Report {
    let cats = root.cats();
    let mode = meta.size_mode;

    let mut special = TopN::new(top);
    let mut files = TopN::new(top);
    walk(root, &mut PathBuf::new(), &mut |n, dedup, p| {
        let list = if n.special {
            &mut special
        } else if !n.is_dir() {
            &mut files
        } else {
            return true;
        };
        let rank = mode.pick(n.size, dedup);
        if list.accepts(|min: &Item| rank > min.rank) {
            list.push(Item::of(n, dedup, p, mode));
        }
        // Nested special dirs only occur inside weak ones (huggingface/ inside
        // .cache/); both are reported.
        true
    });

    let dedup = root.dedup();
    Report {
        path: root_path.display().to_string(),
        size_bytes: root.size,
        size: human(root.size),
        dedup_bytes: dedup,
        clone_shared_bytes: root.size - dedup,
        freeable_min_bytes: root.freeable(),
        accounting: accounting(meta),
        files: root.files(),
        scan_seconds: (meta.elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        cached_at: unix_secs(meta.cached_at),
        unreadable: meta.unreadable,
        categories: shares(&cats, root.size, usize::MAX),
        children: ranked_children(root, mode)
            .into_iter()
            .take(top)
            .map(|i| {
                let c = &root.children()[i];
                let dedup = root.dir.as_ref().map_or(c.size, |d| d.child_dedup(i));
                let total = mode.pick(root.size, root.dedup());
                Child {
                    name: c.name.to_string(),
                    path: root_path.join(&*c.name).display().to_string(),
                    kind: if c.is_dir() { "dir" } else { "file" },
                    size_bytes: c.size,
                    size: human(c.size),
                    percent: round1(mode.pick(c.size, dedup) as f64 * 100.0 / total.max(1) as f64),
                    files: c.files(),
                    special: c.special.then(|| c.cat.label()),
                    top_categories: shares(&c.cats(), c.size, 3),
                    dedup_bytes: dedup,
                    freeable_min_bytes: c.freeable(),
                    clone: c.clone,
                }
            })
            .collect(),
        special_dirs: special.into_sorted().into_iter().map(|i| i.entry(root_path)).collect(),
        largest_files: files.into_sorted().into_iter().map(|i| i.entry(root_path)).collect(),
    }
}

pub fn to_text(r: &Report, root: &Node) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  —  {}  ·  {} files  ·  {:.2}s",
        r.path, r.size, r.files, r.scan_seconds
    );
    if r.clone_shared_bytes > 0 {
        let _ = writeln!(
            out,
            "(APFS clones: {} counted once is {}; deleting it all frees at least {})",
            r.size,
            human(r.dedup_bytes),
            human(r.freeable_min_bytes)
        );
    }
    if let Some(at) = r.cached_at {
        let _ = writeln!(out, "(from cache, scanned {}; --refresh to rescan)", crate::ui::ago(at));
    }
    if r.unreadable > 0 {
        let _ = writeln!(out, "({} entries unreadable)", r.unreadable);
    }
    let _ = writeln!(out, "\nBy category:");
    for c in &r.categories {
        let _ = writeln!(out, "  {:<14} {:>10}  {:5.1}%", c.name, c.size, c.percent);
    }
    let dedup = r.accounting.size_mode == SizeMode::Dedup;
    let shown = |du: u64, d: u64| human(if dedup { d } else { du });
    let _ = writeln!(
        out,
        "\nLargest entries{}:",
        if dedup { " (clones counted once)" } else { "" }
    );
    let order = ranked_children(root, r.accounting.size_mode);
    for (c, &i) in r.children.iter().zip(&order) {
        let node = &root.children()[i];
        let suffix = if node.is_dir() { "/" } else { "" };
        let _ = writeln!(
            out,
            "  {:>10}  {:<14} {}{}",
            shown(c.size_bytes, c.dedup_bytes),
            dominant(&node.cats()).label(),
            c.name,
            suffix
        );
    }
    if !r.special_dirs.is_empty() {
        let _ = writeln!(out, "\nLargest special directories:");
        for e in &r.special_dirs {
            let size = shown(e.size_bytes, e.dedup_bytes);
            let _ = writeln!(out, "  {:>10}  {:<14} {}", size, e.category, e.path);
        }
    }
    let _ = writeln!(out, "\nLargest files:");
    for e in &r.largest_files {
        let size = shown(e.size_bytes, e.dedup_bytes);
        let _ = writeln!(out, "  {:>10}  {:<14} {}", size, e.category, e.path);
    }
    out
}

/// Largest items of one category below `root`: the matching special dirs for
/// directory categories (e.g. every node_modules), files otherwise.
pub fn find(root: &Node, root_path: &Path, cat: Category, top: usize, mode: SizeMode) -> Vec<Entry> {
    let mut list = TopN::new(top);
    let want_dirs = cat.is_dir_kind();
    walk(root, &mut PathBuf::new(), &mut |n, dedup, p| {
        let hit = if want_dirs {
            n.special && n.cat == cat
        } else {
            !n.is_dir() && n.cat == cat
        };
        let rank = mode.pick(n.size, dedup);
        if hit && list.accepts(|min: &Item| rank > min.rank) {
            list.push(Item::of(n, dedup, p, mode));
        }
        !(hit && want_dirs)
    });
    list.into_sorted().into_iter().map(|i| i.entry(root_path)).collect()
}

pub fn find_to_text(cat: Category, items: &[Entry]) -> String {
    use std::fmt::Write as _;
    let total: u64 = items.iter().map(|e| e.size_bytes).sum();
    let mut out = format!(
        "Largest {} ({} shown, {} total):\n",
        cat.label(),
        items.len(),
        human(total)
    );
    for e in items {
        let _ = writeln!(out, "  {:>10}  {}", e.size, e.path);
    }
    out
}
