//! Non-interactive output: a human-readable text report and a JSON document
//! meant for scripts and LLM agents.

use crate::category::{Category, by_size, dominant};
use crate::scan::{Node, PrivateSizes, TopN};
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
pub struct Meta<'a> {
    pub elapsed: Duration,
    pub cached_at: Option<SystemTime>,
    pub unreadable: u64,
    /// The scan looked for APFS clones (`--clones`).
    pub clones: bool,
    /// ... and the filesystem reported them.
    pub clone_ids: bool,
    /// ... but not for every file.
    pub clone_gaps: bool,
    /// Files may share blocks there ([`crate::scan::may_share_blocks`]).
    pub shares: bool,
    pub size_mode: SizeMode,
    /// The scan read APFS private sizes (`--private` on macOS): these.
    pub private: Option<&'a PrivateSizes>,
}

/// `n`'s freeable bytes, when known: always for a subtree without files
/// (only dirs' own blocks), else when `known` (see [`freeable_known`]).
pub fn freeable_of(n: &Node, known: bool) -> Option<u64> {
    (known || n.files() == 0).then(|| n.freeable())
}

/// Whether a lower bound of freed bytes can be given: where files may share
/// blocks (`shares`: APFS, btrfs, XFS, ...), only a scan that detected
/// shared blocks (`--clones` on APFS) can tell.
pub fn freeable_known(detected: bool, shares: bool) -> bool {
    detected || !shares
}

/// How sizes were counted, so agents know what the numbers mean.
#[derive(Serialize)]
pub struct Accounting {
    /// `size_bytes`: allocated bytes, hard links counted once (`du -skx`).
    pub size: &'static str,
    /// What the lists are ranked by.
    pub size_mode: SizeMode,
    /// `apfs-full`: identical APFS clones are detected (`dedup_bytes`,
    /// `freeable_min_bytes`); `apfs-partial`: the same, but some files'
    /// clone flags could not be read (another volume): those are counted
    /// in full and as possibly shared; `unsupported`: they are counted in full and
    /// `dedup_bytes` equals `size_bytes`; `off`: not looked for (no
    /// `--clones`), `dedup_bytes` equals `size_bytes`. Unless clones were
    /// detected, `freeable_min_bytes` is null where files may share blocks
    /// (macOS; btrfs, XFS, bcachefs, OCFS2, ZFS; Linux with `--cross-fs`).
    pub clone_detection: &'static str,
    /// `private_bytes` were collected (`--private`, APFS): on files that may
    /// share blocks but are not full clones, what deleting the file alone
    /// frees right now. Never summed into folders.
    pub private: bool,
    pub caveats: Vec<&'static str>,
}

/// The `accounting` object for a tree scanned with these facts.
pub fn accounting(meta: &Meta) -> Accounting {
    let mut caveats = match (meta.clones, meta.clone_ids, meta.clone_gaps) {
        (false, ..) => vec![
            "clones-not-detected",
            "hard-links-not-freeable",
            "snapshots-not-considered",
        ],
        (true, true, false) => vec!["partial-clones-not-deduplicated", "snapshots-not-considered"],
        (true, true, true) => vec![
            "partial-clones-not-deduplicated",
            "clone-flags-missing-for-some-files",
            "snapshots-not-considered",
        ],
        (true, false, _) => vec!["snapshots-not-considered"],
    };
    if meta.private.is_some() {
        caveats.push("private-bytes-affected-by-snapshots");
    }
    Accounting {
        size: "du-allocated",
        size_mode: meta.size_mode,
        clone_detection: match (meta.clones, meta.clone_ids, meta.clone_gaps) {
            (false, ..) => "off",
            (true, true, false) => "apfs-full",
            (true, true, true) => "apfs-partial",
            (true, false, _) => "unsupported",
        },
        private: meta.private.is_some(),
        caveats,
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
    /// Lower bound of what deleting the whole folder would free; null when
    /// it cannot be known (macOS without `--clones`).
    pub freeable_min_bytes: Option<u64>,
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
    pub freeable_min_bytes: Option<u64>,
    /// It is, or holds, a file that may share blocks with others (APFS clones).
    pub clone: bool,
    /// Files with `--private`: APFS bytes freed immediately if this file
    /// alone is deleted (affected by APFS snapshots). Only on files that may
    /// share blocks but are not full clones (absent if it could not be read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct Entry {
    pub path: String,
    pub category: &'static str,
    pub size_bytes: u64,
    pub size: String,
    pub dedup_bytes: u64,
    pub freeable_min_bytes: Option<u64>,
    pub clone: bool,
    /// See [`Child::private_bytes`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_bytes: Option<u64>,
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
/// on its parent) and private size.
struct Item {
    rank: u64,
    rel: PathBuf,
    cat: Category,
    size: u64,
    dedup: u64,
    freeable: Option<u64>,
    clone: bool,
    private: Option<u64>,
}

impl Item {
    fn of(n: &Node, dedup: u64, private: Option<u64>, rel: &Path, mode: SizeMode, known: bool) -> Item {
        Item {
            rank: mode.pick(n.size, dedup),
            rel: rel.to_path_buf(),
            cat: n.cat,
            size: n.size,
            dedup,
            freeable: freeable_of(n, known),
            clone: n.clone,
            private,
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
            private_bytes: self.private,
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
/// each node's deduplicated size and whether its path has a non-UTF-8
/// (lossy) name.
fn walk(node: &Node, rel: &mut PathBuf, lossy: bool, f: &mut impl FnMut(&Node, u64, &Path, bool) -> bool) {
    let Some(d) = &node.dir else { return };
    for (i, c) in d.children.iter().enumerate() {
        rel.push(&*c.name);
        let lossy = lossy || c.lossy;
        if f(c, d.child_dedup(i), rel, lossy) && c.is_dir() {
            walk(c, rel, lossy, f);
        }
        rel.pop();
    }
}

/// Private size of the file `n` at `rel`, if the scan read one. Never for a
/// path with a lossy name: it renders like a valid name could, and sizes
/// are only kept by valid names.
fn private_of(private: Option<&PrivateSizes>, n: &Node, rel: &Path, lossy: bool) -> Option<u64> {
    private.filter(|_| !n.is_dir() && !lossy)?.get(rel)
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
    let known = freeable_known(meta.clones && meta.clone_ids, meta.shares);
    let cats = root.cats();
    let mode = meta.size_mode;

    let mut special = TopN::new(top);
    let mut files = TopN::new(top);
    walk(root, &mut PathBuf::new(), false, &mut |n, dedup, p, lossy| {
        let list = if n.special {
            &mut special
        } else if !n.is_dir() {
            &mut files
        } else {
            return true;
        };
        let rank = mode.pick(n.size, dedup);
        if list.accepts(|min: &Item| rank > min.rank) {
            list.push(Item::of(
                n,
                dedup,
                private_of(meta.private, n, p, lossy),
                p,
                mode,
                known,
            ));
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
        freeable_min_bytes: freeable_of(root, known),
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
                    freeable_min_bytes: freeable_of(c, known),
                    clone: c.clone,
                    private_bytes: private_of(meta.private, c, Path::new(&*c.name), c.lossy),
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
        let frees = r.freeable_min_bytes.map(human).unwrap_or_else(|| "?".into());
        let _ = writeln!(
            out,
            "(APFS clones: {} counted once is {}; deleting it all frees at least {})",
            r.size,
            human(r.dedup_bytes),
            frees
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
    let _ = writeln!(
        out,
        "\nLargest files{}:",
        if r.accounting.private {
            " (private: freed now if deleted alone)"
        } else {
            ""
        }
    );
    for e in &r.largest_files {
        let size = shown(e.size_bytes, e.dedup_bytes);
        let private = e
            .private_bytes
            .map_or(String::new(), |p| format!("  (private {})", human(p)));
        let _ = writeln!(out, "  {:>10}  {:<14} {}{}", size, e.category, e.path, private);
    }
    out
}

/// Largest items of one category below `root`: the matching special dirs for
/// directory categories (e.g. every node_modules), files otherwise.
/// `known`: freeable bytes can be given (see [`freeable_known`]);
/// `private`: the scan's private sizes, if it read them.
pub fn find(
    root: &Node,
    root_path: &Path,
    cat: Category,
    top: usize,
    mode: SizeMode,
    known: bool,
    private: Option<&PrivateSizes>,
) -> Vec<Entry> {
    let mut list = TopN::new(top);
    let want_dirs = cat.is_dir_kind();
    walk(root, &mut PathBuf::new(), false, &mut |n, dedup, p, lossy| {
        let hit = if want_dirs {
            n.special && n.cat == cat
        } else {
            !n.is_dir() && n.cat == cat
        };
        let rank = mode.pick(n.size, dedup);
        if hit && list.accepts(|min: &Item| rank > min.rank) {
            list.push(Item::of(n, dedup, private_of(private, n, p, lossy), p, mode, known));
        }
        !(hit && want_dirs)
    });
    list.into_sorted().into_iter().map(|i| i.entry(root_path)).collect()
}

/// Text for [`find`]'s items, showing the sizes they were ranked by.
pub fn find_to_text(cat: Category, items: &[Entry], mode: SizeMode) -> String {
    use std::fmt::Write as _;
    let size = |e: &Entry| mode.pick(e.size_bytes, e.dedup_bytes);
    let total: u64 = items.iter().map(size).sum();
    let mut out = format!(
        "Largest {} ({} shown, {} total{}):\n",
        cat.label(),
        items.len(),
        human(total),
        if mode == SizeMode::Dedup {
            ", clones counted once"
        } else {
            ""
        }
    );
    for e in items {
        let private = e
            .private_bytes
            .map_or(String::new(), |p| format!("  (private {})", human(p)));
        let _ = writeln!(out, "  {:>10}  {}{}", human(size(e)), e.path, private);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::reg;

    /// A private size kept for a valid name never shows on a non-UTF-8
    /// file that renders the same (`\xff` and a raw 0xff byte).
    #[cfg(unix)]
    #[test]
    fn private_sizes_skip_lossy_names() {
        use crate::scan::{Progress, scan_excluding};
        let d = std::env::temp_dir().join(format!("dutype-report-lossy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut f = d.clone();
        f.push("\\xff");
        std::fs::write(&f, vec![1u8; 8192]).unwrap();
        std::fs::write(d.join("other"), vec![1u8; 65536]).unwrap();
        let mut root = scan_excluding(&d, true, false, &Progress::default(), None).unwrap();
        // Stand in for a raw 0xff name (APFS refuses to create one).
        let twin = root
            .dir
            .as_mut()
            .unwrap()
            .children
            .iter_mut()
            .find(|c| &*c.name == "other")
            .unwrap();
        (twin.name, twin.lossy) = ("\\xff".into(), true);
        let mut table = PrivateSizes::default();
        table.0.insert(PathBuf::from("\\xff"), 4096);
        let meta = Meta {
            elapsed: Duration::ZERO,
            cached_at: None,
            unreadable: 0,
            clones: true,
            clone_ids: true,
            clone_gaps: false,
            shares: true,
            size_mode: SizeMode::Du,
            private: Some(&table),
        };
        let r = build(&root, &d, &meta, 10);
        let by_size = |l: &[Entry], size: u64| l.iter().find(|e| e.size_bytes == size).unwrap().private_bytes;
        let small = root.children().iter().find(|c| !c.lossy).unwrap().size;
        let big = root.children().iter().find(|c| c.lossy).unwrap().size;
        assert_eq!(by_size(&r.largest_files, small), Some(4096));
        assert_eq!(by_size(&r.largest_files, big), None);
        let kids: Vec<_> = r.children.iter().map(|c| (c.size_bytes, c.private_bytes)).collect();
        assert!(kids.contains(&(small, Some(4096))) && kids.contains(&(big, None)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Detection is "apfs-full" only when every file's clone flags were read.
    #[test]
    fn clone_detection_states() {
        let state = |clones, clone_ids, clone_gaps| {
            let meta = Meta {
                elapsed: Duration::ZERO,
                cached_at: None,
                unreadable: 0,
                clones,
                clone_ids,
                clone_gaps,
                shares: true,
                size_mode: SizeMode::Du,
                private: None,
            };
            accounting(&meta).clone_detection
        };
        assert_eq!(state(false, false, false), "off");
        assert_eq!(state(true, true, false), "apfs-full");
        assert_eq!(state(true, true, true), "apfs-partial");
        assert_eq!(state(true, false, true), "unsupported");
    }

    #[test]
    fn category_text_shows_the_ranked_size() {
        let item = |path: &str, size: u64, dedup: u64| Entry {
            path: path.into(),
            category: reg().other().label(),
            size_bytes: size,
            size: human(size),
            dedup_bytes: dedup,
            freeable_min_bytes: None,
            clone: dedup < size,
            private_bytes: None,
        };
        let items = [item("/a", 3 << 20, 3 << 20), item("/b", 2 << 20, 0)];
        let du = find_to_text(reg().other(), &items, SizeMode::Du);
        assert!(du.contains("5.0 MiB total)") && du.contains("2.0 MiB  /b"), "{du}");
        let dedup = find_to_text(reg().other(), &items, SizeMode::Dedup);
        assert!(dedup.contains("3.0 MiB total, clones counted once)"), "{dedup}");
        assert!(dedup.contains("0 B  /b"), "{dedup}");
    }
}
