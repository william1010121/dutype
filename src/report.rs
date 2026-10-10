//! Non-interactive output: a human-readable text report and a JSON document
//! meant for scripts and LLM agents.

use crate::category::{Category, by_size, dominant};
use crate::scan::{Node, TopN, walk};
use crate::ui::human;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Serialize)]
pub struct Report {
    /// Absolute path that was scanned.
    pub path: String,
    pub size_bytes: u64,
    pub size: String,
    pub files: u64,
    pub scan_seconds: f64,
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
}

#[derive(Serialize)]
pub struct Entry {
    pub path: String,
    pub category: &'static str,
    pub size_bytes: u64,
    pub size: String,
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

fn entry(root: &Path, rel: PathBuf, size: u64, cat: Category) -> Entry {
    Entry {
        path: root.join(rel).display().to_string(),
        category: cat.label(),
        size_bytes: size,
        size: human(size),
    }
}

pub fn build(root: &Node, root_path: &Path, elapsed: Duration, unreadable: u64, top: usize) -> Report {
    let cats = root.cats();

    let mut special = TopN::new(top);
    let mut files = TopN::new(top);
    walk(root, &mut PathBuf::new(), &mut |n, p| {
        let list = if n.special {
            &mut special
        } else if !n.is_dir() {
            &mut files
        } else {
            return true;
        };
        if list.accepts(|min: &(u64, PathBuf, Category)| n.size > min.0) {
            list.push((n.size, p.to_path_buf(), n.cat));
        }
        // Nested special dirs only occur inside weak ones (huggingface/ inside
        // .cache/); both are reported.
        true
    });

    Report {
        path: root_path.display().to_string(),
        size_bytes: root.size,
        size: human(root.size),
        files: root.files(),
        scan_seconds: (elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        unreadable,
        categories: shares(&cats, root.size, usize::MAX),
        children: root
            .children()
            .iter()
            .take(top)
            .map(|c| Child {
                name: c.name.to_string(),
                path: root_path.join(&*c.name).display().to_string(),
                kind: if c.is_dir() { "dir" } else { "file" },
                size_bytes: c.size,
                size: human(c.size),
                percent: round1(c.size as f64 * 100.0 / root.size.max(1) as f64),
                files: c.files(),
                special: c.special.then(|| c.cat.label()),
                top_categories: shares(&c.cats(), c.size, 3),
            })
            .collect(),
        special_dirs: special
            .into_sorted()
            .into_iter()
            .map(|(s, p, c)| entry(root_path, p, s, c))
            .collect(),
        largest_files: files
            .into_sorted()
            .into_iter()
            .map(|(s, p, c)| entry(root_path, p, s, c))
            .collect(),
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
    if r.unreadable > 0 {
        let _ = writeln!(out, "({} entries unreadable)", r.unreadable);
    }
    let _ = writeln!(out, "\nBy category:");
    for c in &r.categories {
        let _ = writeln!(out, "  {:<14} {:>10}  {:5.1}%", c.name, c.size, c.percent);
    }
    let _ = writeln!(out, "\nLargest entries:");
    for (c, node) in r.children.iter().zip(root.children()) {
        let suffix = if node.is_dir() { "/" } else { "" };
        let _ = writeln!(
            out,
            "  {:>10}  {:<14} {}{}",
            c.size,
            dominant(&node.cats()).label(),
            c.name,
            suffix
        );
    }
    if !r.special_dirs.is_empty() {
        let _ = writeln!(out, "\nLargest special directories:");
        for e in &r.special_dirs {
            let _ = writeln!(out, "  {:>10}  {:<14} {}", e.size, e.category, e.path);
        }
    }
    let _ = writeln!(out, "\nLargest files:");
    for e in &r.largest_files {
        let _ = writeln!(out, "  {:>10}  {:<14} {}", e.size, e.category, e.path);
    }
    out
}

/// Largest items of one category below `root`: the matching special dirs for
/// directory categories (e.g. every node_modules), files otherwise.
pub fn find(root: &Node, root_path: &Path, cat: Category, top: usize) -> Vec<Entry> {
    let mut list = TopN::new(top);
    let want_dirs = cat.is_dir_kind();
    walk(root, &mut PathBuf::new(), &mut |n, p| {
        let hit = if want_dirs {
            n.special && n.cat == cat
        } else {
            !n.is_dir() && n.cat == cat
        };
        if hit && list.accepts(|min: &(u64, PathBuf)| n.size > min.0) {
            list.push((n.size, p.to_path_buf()));
        }
        !(hit && want_dirs)
    });
    list.into_sorted()
        .into_iter()
        .map(|(s, p)| entry(root_path, p, s, cat))
        .collect()
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
