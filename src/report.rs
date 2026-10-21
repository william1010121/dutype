//! Non-interactive output: a human-readable text report and a JSON document
//! meant for scripts and LLM agents.

use crate::category::{Category, by_size, dominant};
use crate::display;
use crate::scan::{Node, PrivateSizes, ReflinkFs, TopN};
use crate::ui::human;
use serde::Serialize;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Which size lists are ranked by (and the text report shows).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    /// Allocated bytes, like `du -skx` (hard links once).
    #[default]
    Du,
    /// Like `du`, but each APFS clone family (or, on Linux, each shared
    /// extent) counted once.
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
    /// Scan-wide unreadable count, even for a single-category report: an
    /// unreadable entry may not have a known category.
    pub unreadable: u64,
    /// The scan looked for APFS clones (`--clones`).
    pub clones: bool,
    /// ... and the filesystem reported them.
    pub clone_ids: bool,
    /// ... but not for every file.
    pub clone_gaps: bool,
    /// Files may share blocks there ([`crate::scan::may_share_blocks`]).
    pub shares: bool,
    /// With `clones`, the shared extents of this Linux reflink filesystem
    /// were read.
    pub reflink_fs: Option<ReflinkFs>,
    pub size_mode: SizeMode,
    /// The scan read APFS private sizes (`--private` on macOS): these.
    pub private: Option<&'a PrivateSizes>,
}

/// `n`'s freeable bytes, when known: always for a subtree without files
/// (only dirs' own blocks), else when `known` (see [`freeable_known`]).
pub fn freeable_of(n: &Node, known: bool) -> Option<u64> {
    (known || n.files() == 0).then(|| n.freeable())
}

/// Shared blocks were detected: APFS reported clone flags, or (with
/// `--clones` on btrfs, XFS, bcachefs) shared extents were read.
pub fn detected(clones: bool, clone_ids: bool, reflink_fs: Option<ReflinkFs>) -> bool {
    clones && (clone_ids || reflink_fs.is_some())
}

/// Whether a lower bound of freed bytes can be given: where files may share
/// blocks (`shares`: APFS, btrfs, XFS, ...), only a scan that detected
/// shared blocks (see [`detected`]) can tell.
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
    /// in full and as possibly shared; `fiemap`: shared extents were read on
    /// a Linux reflink filesystem (btrfs, XFS, bcachefs), each shared
    /// physical byte counts once in `dedup_bytes` and never in
    /// `freeable_min_bytes`; `fiemap-partial`: the same, but some files could
    /// not be probed (counted in full and as possibly shared);
    /// `unsupported`: shared blocks are counted in full and `dedup_bytes`
    /// equals `size_bytes`; `off`: not looked for (no `--clones`),
    /// `dedup_bytes` equals `size_bytes`. Unless shared blocks were
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
    let mut caveats = match (meta.clones, meta.reflink_fs, meta.clone_ids, meta.clone_gaps) {
        (false, ..) => vec![
            "clones-not-detected",
            "hard-links-not-freeable",
            "snapshots-not-considered",
        ],
        (true, Some(fs), _, gaps) => {
            let mut c = vec!["shared-extents-not-freeable"];
            if fs == ReflinkFs::Btrfs {
                c.extend(["snapshots-counted-as-shared", "compressed-extents-not-deduplicated"]);
            }
            if gaps {
                c.push("shared-extents-missing-for-some-files");
            }
            c
        }
        (true, None, true, false) => vec!["partial-clones-not-deduplicated", "snapshots-not-considered"],
        (true, None, true, true) => vec![
            "partial-clones-not-deduplicated",
            "clone-flags-missing-for-some-files",
            "snapshots-not-considered",
        ],
        (true, None, false, _) => vec!["snapshots-not-considered"],
    };
    if meta.private.is_some() {
        caveats.push("private-bytes-affected-by-snapshots");
    }
    Accounting {
        size: "du-allocated",
        size_mode: meta.size_mode,
        clone_detection: match (meta.clones, meta.reflink_fs, meta.clone_ids, meta.clone_gaps) {
            (false, ..) => "off",
            (true, Some(_), _, false) => "fiemap",
            (true, Some(_), _, true) => "fiemap-partial",
            (true, None, true, false) => "apfs-full",
            (true, None, true, true) => "apfs-partial",
            (true, None, false, _) => "unsupported",
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
    /// it cannot be known (macOS or a Linux reflink filesystem without
    /// `--clones`).
    pub freeable_min_bytes: Option<u64>,
    pub accounting: Accounting,
    pub files: u64,
    pub scan_seconds: f64,
    /// Unix time of the scan when it was read from the `--cache`, else null.
    pub cached_at: Option<u64>,
    /// Scan-wide count of entries that could not be read (permissions, races).
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
    /// It is, or holds, a file that may share blocks with others (APFS
    /// clones, shared extents).
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
    fn of(n: &Node, s: Sizes, private: Option<u64>, rel: &Path, mode: SizeMode, known: bool) -> Item {
        Item {
            rank: mode.pick(n.size, s.dedup),
            rel: rel.to_path_buf(),
            cat: n.cat,
            size: n.size,
            dedup: s.dedup,
            freeable: freeable_of(n, known).map(|_| s.freeable),
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

/// Sizes of a node that, for a file, depend on its parent's records.
#[derive(Clone, Copy)]
struct Sizes {
    dedup: u64,
    freeable: u64,
}

/// Depth-first walk below `node` like [`crate::scan::walk`], also passing
/// each node's deduplicated and freeable sizes and whether its path has a
/// non-UTF-8 (lossy) name.
fn walk(node: &Node, rel: &mut PathBuf, lossy: bool, f: &mut impl FnMut(&Node, Sizes, &Path, bool) -> bool) {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let Some(d) = &node.dir else { return };
        // Children are sorted by descending size, ascending name. Visit
        // equal-size siblings in reverse name order to match Item's tie
        // breaker, avoiding a heap replacement/allocation for every tied file.
        let mut start = 0;
        while start < d.children.len() {
            let mut end = start + 1;
            while end < d.children.len() && d.children[end].size == d.children[start].size {
                end += 1;
            }
            for i in (start..end).rev() {
                let c = &d.children[i];
                rel.push(&*c.name);
                let lossy = lossy || c.lossy;
                let sizes = Sizes {
                    dedup: d.child_dedup(i),
                    freeable: d.child_freeable(i),
                };
                if f(c, sizes, rel, lossy) && c.is_dir() {
                    walk(c, rel, lossy, f);
                }
                rel.pop();
            }
            start = end;
        }
    });
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
    let known = freeable_known(detected(meta.clones, meta.clone_ids, meta.reflink_fs), meta.shares);
    let cats = root.cats();
    let mode = meta.size_mode;

    let mut special = TopN::new(top);
    let mut files = TopN::new(top);
    walk(root, &mut PathBuf::new(), false, &mut |n, sizes, p, lossy| {
        let list = if n.special {
            &mut special
        } else if !n.is_dir() {
            &mut files
        } else {
            return true;
        };
        let rank = mode.pick(n.size, sizes.dedup);
        if list.accepts(|min: &Item| rank > min.rank || (rank == min.rank && p > min.rel.as_path())) {
            list.push(Item::of(
                n,
                sizes,
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
                    freeable_min_bytes: freeable_of(c, known)
                        .map(|_| root.dir.as_ref().map_or(0, |d| d.child_freeable(i))),
                    clone: c.clone,
                    private_bytes: private_of(meta.private, c, Path::new(&*c.name), c.lossy),
                }
            })
            .collect(),
        special_dirs: special.into_sorted().into_iter().map(|i| i.entry(root_path)).collect(),
        largest_files: files.into_sorted().into_iter().map(|i| i.entry(root_path)).collect(),
    }
}

/// What the text report calls shared blocks, by `clone_detection`: every
/// FIEMAP state (`fiemap`, `fiemap-partial`) reads shared extents.
fn shared_label(clone_detection: &str) -> &'static str {
    match clone_detection.starts_with("fiemap") {
        true => "shared extents",
        false => "APFS clones",
    }
}

/// Snapshot provenance and scan-wide errors, shared by full/category text.
fn scan_notices(out: &mut String, cached_at: Option<u64>, unreadable: u64) {
    use std::fmt::Write as _;
    if let Some(at) = cached_at {
        let _ = writeln!(out, "(from cache, scanned {}; --refresh to rescan)", crate::ui::ago(at));
    }
    if unreadable > 0 {
        let _ = writeln!(out, "({unreadable} entries unreadable)");
    }
}

pub fn to_text(r: &Report, root: &Node) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  —  {}  ·  {} files  ·  {:.2}s",
        display::path_text(&r.path),
        r.size,
        r.files,
        r.scan_seconds
    );
    if r.clone_shared_bytes > 0 {
        let frees = r.freeable_min_bytes.map(human).unwrap_or_else(|| "?".into());
        let _ = writeln!(
            out,
            "({}: {} counted once is {}; deleting it all frees at least {})",
            shared_label(r.accounting.clone_detection),
            r.size,
            human(r.dedup_bytes),
            frees
        );
    }
    scan_notices(&mut out, r.cached_at, r.unreadable);
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
            display::name(&c.name),
            suffix
        );
    }
    if !r.special_dirs.is_empty() {
        let _ = writeln!(out, "\nLargest special directories:");
        for e in &r.special_dirs {
            let size = shown(e.size_bytes, e.dedup_bytes);
            let _ = writeln!(
                out,
                "  {:>10}  {:<14} {}",
                size,
                e.category,
                display::path_text(&e.path)
            );
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
        let _ = writeln!(
            out,
            "  {:>10}  {:<14} {}{}",
            size,
            e.category,
            display::path_text(&e.path),
            private
        );
    }
    out
}

/// Matches in the scanned snapshot, including items omitted by the list limit.
/// Matching directories count once and prune their descendants. Unreadable
/// entries cannot be inferred from this count; see [`Meta::unreadable`].
pub struct CategoryMatches {
    pub matched_items: u64,
    pub items_truncated: bool,
    pub items: Vec<Entry>,
}

/// Matching policy shared by bounded category reports and cursor pages.
/// The default remains directory-only for categories with directory rules.
#[derive(Clone, Copy)]
pub struct CategoryQuery {
    cat: Category,
    want_dirs: bool,
    include_files: bool,
}

impl CategoryQuery {
    pub fn new(cat: Category, include_files: bool) -> Self {
        let want_dirs = cat.is_dir_kind();
        Self {
            cat,
            want_dirs,
            // Resolve active extension ownership once, only for the opt-in.
            include_files: include_files && want_dirs && crate::category::reg().has_file_rules(cat),
        }
    }

    fn matches(self, node: &Node) -> bool {
        if !self.want_dirs {
            return !node.is_dir() && node.cat == self.cat;
        }
        node.cat == self.cat
            && (node.special
                || (self.include_files
                    && !node.is_dir()
                    && crate::category::reg().classify_file(&node.name) == self.cat))
    }
}

impl From<Category> for CategoryQuery {
    fn from(cat: Category) -> Self {
        Self::new(cat, false)
    }
}

/// Largest items of one category below `root`: the matching special dirs for
/// directory categories (e.g. every node_modules), files otherwise. An opt-in
/// [`CategoryQuery`] also includes directly classified files for mixed categories.
/// `known`: freeable bytes can be given (see [`freeable_known`]);
/// `private`: the scan's private sizes, if it read them.
pub fn find(
    root: &Node,
    root_path: &Path,
    query: impl Into<CategoryQuery>,
    top: usize,
    mode: SizeMode,
    known: bool,
    private: Option<&PrivateSizes>,
) -> CategoryMatches {
    let mut list = TopN::new(top);
    let mut matched_items = 0u64;
    let query = query.into();
    walk(root, &mut PathBuf::new(), false, &mut |n, sizes, p, lossy| {
        let hit = query.matches(n);
        let rank = mode.pick(n.size, sizes.dedup);
        if hit {
            matched_items = matched_items.saturating_add(1);
            if list.accepts(|min: &Item| rank > min.rank || (rank == min.rank && p > min.rel.as_path())) {
                list.push(Item::of(n, sizes, private_of(private, n, p, lossy), p, mode, known));
            }
        }
        !(hit && n.is_dir())
    });
    let items: Vec<_> = list.into_sorted().into_iter().map(|i| i.entry(root_path)).collect();
    CategoryMatches {
        matched_items,
        items_truncated: u64::try_from(items.len()).is_ok_and(|shown| matched_items > shown),
        items,
    }
}

/// One bounded page from an immutable scan. `items_truncated` retains the
/// legacy meaning (total matches exceed this page's length), not `has_more`.
pub struct CategoryPage {
    pub matched_items: u64,
    pub items_truncated: bool,
    pub items: Vec<Entry>,
    pub offset: u64,
    pub has_more: bool,
    /// Last returned visit ordinal, only when another page remains.
    pub next_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CategoryPageError {
    InvalidTop,
    InvalidBoundary,
    CountOverflow,
}

impl std::fmt::Display for CategoryPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidTop => "page size must be an integer from 1 to 500",
            Self::InvalidBoundary => "cursor boundary does not identify a category match in this snapshot",
            Self::CountOverflow => "category page count exceeds the supported range",
        })
    }
}

impl std::error::Error for CategoryPageError {}

/// Preserve the legacy rank/path ordering. Only exact collisions use visit
/// order: the earlier visit wins, avoiding replacements for otherwise equal
/// candidates. Ordinals are local to this snapshot and category traversal.
fn page_cmp(rank: u64, rel: &Path, ordinal: u64, other_rank: u64, other_rel: &Path, other: u64) -> Ordering {
    rank.cmp(&other_rank)
        .then_with(|| rel.cmp(other_rel))
        .then_with(|| other.cmp(&ordinal))
}

struct PageItem {
    item: Item,
    ordinal: u64,
}

impl PartialEq for PageItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for PageItem {}
impl PartialOrd for PageItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PageItem {
    fn cmp(&self, other: &Self) -> Ordering {
        page_cmp(
            self.item.rank,
            &self.item.rel,
            self.ordinal,
            other.item.rank,
            &other.item.rel,
            other.ordinal,
        )
    }
}

fn page_increment(value: &mut u64) -> Result<(), CategoryPageError> {
    *value = value.checked_add(1).ok_or(CategoryPageError::CountOverflow)?;
    Ok(())
}

/// Use the existing stack-safe walk and exactly the category pruning of
/// `find`. Ordinals count all visited nodes, not only category matches, with
/// zero reserved for an invalid boundary. A rejected match still prunes.
fn walk_page(
    root: &Node,
    query: CategoryQuery,
    mut visit: impl FnMut(&Node, Sizes, &Path, bool, u64) -> Result<(), CategoryPageError>,
) -> Result<(), CategoryPageError> {
    #[cfg(test)]
    page_metrics::record(|m| m.walks += 1);
    let mut ordinal = 0;
    let mut error = None;
    walk(root, &mut PathBuf::new(), false, &mut |n, sizes, rel, lossy| {
        if error.is_some() {
            return false;
        }
        if let Err(e) = page_increment(&mut ordinal) {
            error = Some(e);
            return false;
        }
        #[cfg(test)]
        page_metrics::record(|m| m.visited_nodes += 1);
        let hit = query.matches(n);
        if hit {
            error = visit(n, sizes, rel, lossy, ordinal).err();
        }
        !(hit && n.is_dir())
    });
    error.map_or(Ok(()), Err)
}

/// Select at most 500 matches without materializing a full result or an
/// offset-sized prefix. The caller must validate the process-local cursor
/// fingerprint and snapshot/query binding before passing its boundary ordinal here.
///
/// A first page takes one tree walk. A continuation first recovers one owned
/// boundary path, rejecting nonexistent/nonmatching ordinals before building
/// any candidates, then walks again to count and select the best `top` items
/// strictly after it. Workspace is O(depth + path length + top * path length).
#[allow(clippy::too_many_arguments)]
pub fn find_page(
    root: &Node,
    root_path: &Path,
    query: impl Into<CategoryQuery>,
    top: usize,
    mode: SizeMode,
    known: bool,
    private: Option<&PrivateSizes>,
    after: Option<u64>,
) -> Result<CategoryPage, CategoryPageError> {
    if !(1..=500).contains(&top) {
        return Err(CategoryPageError::InvalidTop);
    }
    if after == Some(0) {
        return Err(CategoryPageError::InvalidBoundary);
    }
    let query = query.into();
    let boundary = if let Some(after) = after {
        let mut boundary = None;
        walk_page(root, query, |n, sizes, rel, _, ordinal| {
            if ordinal == after {
                #[cfg(test)]
                page_metrics::record(|m| m.boundary_paths += 1);
                boundary = Some((mode.pick(n.size, sizes.dedup), rel.to_path_buf(), ordinal));
            }
            Ok(())
        })?;
        Some(boundary.ok_or(CategoryPageError::InvalidBoundary)?)
    } else {
        None
    };

    let mut list: BinaryHeap<Reverse<PageItem>> = BinaryHeap::with_capacity(top);
    let mut matched_items = 0;
    let mut offset = 0;
    walk_page(root, query, |n, sizes, rel, lossy, ordinal| {
        page_increment(&mut matched_items)?;
        let rank = mode.pick(n.size, sizes.dedup);
        if let Some((boundary_rank, boundary_rel, boundary_ordinal)) = &boundary
            && !page_cmp(rank, rel, ordinal, *boundary_rank, boundary_rel, *boundary_ordinal).is_lt()
        {
            page_increment(&mut offset)?;
            return Ok(());
        }
        // The same accepts-before-owned-path invariant as TopN. Evict first
        // so even a replacement has at most `top` live candidate paths.
        let accepts = list.len() < top
            || list.peek().is_some_and(|Reverse(min)| {
                page_cmp(rank, rel, ordinal, min.item.rank, &min.item.rel, min.ordinal).is_gt()
            });
        if accepts {
            if list.len() == top {
                list.pop();
            }
            list.push(Reverse(PageItem {
                item: Item::of(n, sizes, private_of(private, n, rel, lossy), rel, mode, known),
                ordinal,
            }));
            #[cfg(test)]
            page_metrics::record(|m| {
                m.candidate_paths += 1;
                m.max_live_candidates = m.max_live_candidates.max(list.len());
            });
        }
        Ok(())
    })?;
    let mut selected: Vec<_> = list.into_iter().map(|Reverse(item)| item).collect();
    selected.sort_unstable_by(|a, b| b.cmp(a));
    let shown = u64::try_from(selected.len()).map_err(|_| CategoryPageError::CountOverflow)?;
    let has_more = offset.checked_add(shown).ok_or(CategoryPageError::CountOverflow)? < matched_items;
    let next_after = if has_more {
        selected.last().map(|item| item.ordinal)
    } else {
        None
    };
    let mut items = Vec::with_capacity(selected.len());
    for selected in selected {
        items.push(selected.item.entry(root_path));
        #[cfg(test)]
        page_metrics::record(|m| m.max_output_entries = m.max_output_entries.max(items.len()));
    }
    Ok(CategoryPage {
        matched_items,
        items_truncated: matched_items > shown,
        items,
        offset,
        has_more,
        next_after,
    })
}

#[cfg(test)]
mod page_metrics {
    use std::cell::RefCell;

    #[derive(Clone, Copy, Default, Debug)]
    pub(super) struct Metrics {
        pub walks: usize,
        pub visited_nodes: usize,
        pub boundary_paths: usize,
        pub candidate_paths: usize,
        pub max_live_candidates: usize,
        pub max_output_entries: usize,
    }

    thread_local! {
        static METRICS: RefCell<Metrics> = RefCell::new(Metrics::default());
    }

    pub(super) fn record(update: impl FnOnce(&mut Metrics)) {
        METRICS.with(|m| update(&mut m.borrow_mut()));
    }

    pub(super) fn reset() {
        record(|m| *m = Metrics::default());
    }

    pub(super) fn get() -> Metrics {
        METRICS.with(|m| *m.borrow())
    }
}

/// Text for [`find`]'s items, showing the sizes they were ranked by and any
/// cached-snapshot/scan-wide unreadable notices.
pub fn find_to_text(cat: Category, found: &CategoryMatches, meta: &Meta) -> String {
    use std::fmt::Write as _;
    let items = &found.items;
    let mode = meta.size_mode;
    let size = |e: &Entry| mode.pick(e.size_bytes, e.dedup_bytes);
    let total: u64 = items.iter().map(size).sum();
    let mut out = String::new();
    scan_notices(&mut out, unix_secs(meta.cached_at), meta.unreadable);
    let _ = writeln!(
        out,
        "Largest {} ({} of {} shown, {} displayed total{}):",
        cat.label(),
        items.len(),
        found.matched_items,
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
        let _ = writeln!(
            out,
            "  {:>10}  {}{}",
            human(size(e)),
            display::path_text(&e.path),
            private
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::reg;

    fn plain_meta() -> Meta<'static> {
        Meta {
            elapsed: Duration::ZERO,
            cached_at: None,
            unreadable: 0,
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            shares: false,
            size_mode: SizeMode::Du,
            private: None,
            reflink_fs: None,
        }
    }

    fn file(name: &str, size: u64, cat: Category) -> Node {
        Node {
            name: name.into(),
            size,
            cat,
            special: false,
            hardlink: false,
            lossy: false,
            clone: false,
            dir: None,
        }
    }

    fn directory(name: &str, cat: Category, special: bool, children: Vec<Node>) -> Node {
        let mut root = file(name, children.iter().map(|n| n.size).sum(), cat);
        root.special = special;
        root.dir = Some(Box::new(crate::scan::DirInfo {
            cats: vec![0; reg().len()].into_boxed_slice(),
            files: children.iter().map(Node::files).sum(),
            errors: 0,
            children,
            free_base: root.size,
            clones: None,
        }));
        root.dir.as_mut().unwrap().sort_children(false);
        root
    }

    #[test]
    fn category_matches_count_zero_size_files_independently_of_limit_and_size_mode() {
        use crate::scan::Shared;

        let cat = reg().find_by_name("PDF").unwrap();
        let mut root = directory(
            "root",
            reg().other(),
            false,
            vec![file("a.pdf", 100, cat), file("b.pdf", 50, cat), file("c.pdf", 0, cat)],
        );
        // Simulate a clone whose bytes are attributed elsewhere: dedup ranking
        // changes, while the matching zero-byte and duplicate files still count.
        root.dir.as_mut().unwrap().clones_mut().shared = vec![Shared {
            idx: 0,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 2,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            let expected = if mode == SizeMode::Du {
                ["a.pdf", "b.pdf", "c.pdf"]
            } else {
                ["b.pdf", "c.pdf", "a.pdf"]
            };
            for top in [0, 1, 2, 3, 4, usize::MAX] {
                let found = find(&root, Path::new("root"), cat, top, mode, true, None);
                assert_eq!(found.matched_items, 3);
                assert_eq!(found.items_truncated, top < 3);
                let paths: Vec<_> = found.items.iter().map(|e| PathBuf::from(&e.path)).collect();
                let expected: Vec<_> = expected
                    .iter()
                    .take(top)
                    .map(|name| Path::new("root").join(name))
                    .collect();
                assert_eq!(paths, expected, "{mode:?}, top {top}");
            }
        }
    }

    #[test]
    fn category_matches_prune_matching_directories_even_when_not_retained() {
        let cat = reg().find_by_name("node_modules").unwrap();
        let root = directory(
            "root",
            cat,
            true,
            vec![
                directory("a", cat, true, vec![directory("nested", cat, true, vec![])]),
                directory(
                    "ordinary",
                    reg().other(),
                    false,
                    vec![directory("b", cat, true, vec![])],
                ),
                file("inherited", 0, cat),
            ],
        );
        for top in [0, 1, 2, usize::MAX] {
            let found = find(&root, Path::new("root"), cat, top, SizeMode::Du, true, None);
            // Root is excluded, matching descendants prune regardless of top,
            // and a file inheriting a directory category is not a match.
            assert_eq!(found.matched_items, 2);
            assert_eq!(found.items.len(), top.min(2));
            assert_eq!(found.items_truncated, top < 2);
            assert!(found.items.iter().all(|e| !e.path.contains("nested")));
        }
    }

    fn page_items(page: &CategoryPage) -> serde_json::Value {
        serde_json::to_value(&page.items).unwrap()
    }

    fn assert_page_bounds(top: usize, walks: usize) {
        let metrics = page_metrics::get();
        assert_eq!(metrics.walks, walks, "{metrics:?}");
        assert!(metrics.boundary_paths <= usize::from(walks == 2), "{metrics:?}");
        assert!(metrics.max_live_candidates <= top, "{metrics:?}");
        assert!(metrics.max_output_entries <= top, "{metrics:?}");
    }

    #[test]
    fn category_pages_partition_600_matches_replay_and_change_length() {
        let cat = reg().find_by_name("node_modules").unwrap();
        let root = directory(
            "root",
            reg().other(),
            false,
            (0..600)
                .map(|i| {
                    directory(
                        &format!("p{i:03}"),
                        cat,
                        true,
                        vec![file("inherited", (i % 11) as u64, cat)],
                    )
                })
                .collect(),
        );
        let full = find(&root, Path::new("root"), cat, 600, SizeMode::Du, true, None);
        let expected = serde_json::to_value(&full.items).unwrap();
        for top in [500, 137] {
            let mut after = None;
            let mut collected = Vec::new();
            let mut lengths = Vec::new();
            loop {
                page_metrics::reset();
                let page = find_page(&root, Path::new("root"), cat, top, SizeMode::Du, true, None, after).unwrap();
                assert_page_bounds(top, if after.is_some() { 2 } else { 1 });
                assert_eq!(page_metrics::get().candidate_paths, page.items.len());
                assert_eq!(page.matched_items, 600);
                assert_eq!(page.offset as usize, collected.len());
                assert!(page.items_truncated);
                let replay = find_page(&root, Path::new("root"), cat, top, SizeMode::Du, true, None, after).unwrap();
                assert_eq!(page_items(&replay), page_items(&page));
                assert_eq!(replay.offset, page.offset);
                assert_eq!(replay.next_after, page.next_after);
                lengths.push(page.items.len());
                collected.extend(page_items(&page).as_array().unwrap().iter().cloned());
                assert_eq!(page.has_more, collected.len() < 600);
                assert_eq!(page.next_after.is_some(), page.has_more);
                after = page.next_after;
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(serde_json::Value::Array(collected), expected);
            assert_eq!(
                lengths,
                if top == 500 {
                    vec![500, 100]
                } else {
                    vec![137, 137, 137, 137, 52]
                }
            );
        }
        let first = find_page(&root, Path::new("root"), cat, 137, SizeMode::Du, true, None, None).unwrap();
        let rest = find_page(
            &root,
            Path::new("root"),
            cat,
            500,
            SizeMode::Du,
            true,
            None,
            first.next_after,
        )
        .unwrap();
        assert_eq!(rest.offset, 137);
        assert_eq!(rest.items.len(), 463);
        assert!(!rest.has_more);
        let mut joined = page_items(&first).as_array().unwrap().clone();
        joined.extend(page_items(&rest).as_array().unwrap().iter().cloned());
        assert_eq!(serde_json::Value::Array(joined), expected);
    }

    #[test]
    fn category_page_boundaries_follow_rank_not_walk_position_and_keep_only_one_page() {
        let cat = reg().find_by_name("PDF").unwrap();
        // Walk order is the reverse of match rank: larger containing folders
        // carry smaller matching files. Every late candidate replaces an early
        // one, exercising the live-path bound rather than only a sorted input.
        let root = directory(
            "root",
            reg().other(),
            false,
            (0..600)
                .map(|i| {
                    directory(
                        &format!("p{i:03}"),
                        reg().other(),
                        false,
                        vec![file("hit.pdf", i, cat), file("padding", 2000 - 2 * i, reg().other())],
                    )
                })
                .collect(),
        );
        let full = find(&root, Path::new("root"), cat, 600, SizeMode::Du, true, None);
        page_metrics::reset();
        let first = find_page(&root, Path::new("root"), cat, 500, SizeMode::Du, true, None, None).unwrap();
        assert_page_bounds(500, 1);
        assert_eq!(page_metrics::get().candidate_paths, 600);
        page_metrics::reset();
        let last = find_page(
            &root,
            Path::new("root"),
            cat,
            7,
            SizeMode::Du,
            true,
            None,
            first.next_after,
        )
        .unwrap();
        assert_page_bounds(7, 2);
        assert_eq!(page_metrics::get().candidate_paths, 100);
        assert_eq!(page_metrics::get().max_live_candidates, 7);
        assert_eq!(last.offset, 500);
        assert_eq!(last.items.len(), 7);
        assert_eq!(page_items(&last), serde_json::to_value(&full.items[500..507]).unwrap(),);
        assert!(last.has_more);
    }

    #[test]
    fn category_pages_preserve_ties_collisions_private_and_lossy_ancestry() {
        let cat = reg().find_by_name("PDF").unwrap();
        let mut lossy = file("same.pdf", 40, cat);
        lossy.lossy = true;
        let mut parent = directory("lossy", reg().other(), false, vec![file("child.pdf", 5, cat)]);
        parent.lossy = true;
        let root = directory(
            "root",
            reg().other(),
            false,
            vec![
                file("same.pdf", 40, cat),
                lossy,
                file("a.pdf", 40, cat),
                file("z.pdf", 40, cat),
                parent,
                file("zero.pdf", 0, cat),
            ],
        );
        let mut private = PrivateSizes::default();
        private.0.insert(PathBuf::from("same.pdf"), 9);
        private.0.insert(Path::new("lossy").join("child.pdf"), 3);
        let mut after = None;
        let mut entries = Vec::new();
        let mut ordinals = Vec::new();
        loop {
            let page = find_page(
                &root,
                Path::new("root"),
                cat,
                1,
                SizeMode::Du,
                true,
                Some(&private),
                after,
            )
            .unwrap();
            assert_eq!(page.matched_items, 6);
            assert_eq!(page.offset as usize, entries.len());
            assert!(page.items_truncated);
            let replay = find_page(
                &root,
                Path::new("root"),
                cat,
                1,
                SizeMode::Du,
                true,
                Some(&private),
                after,
            )
            .unwrap();
            assert_eq!(page_items(&replay), page_items(&page));
            after = page.next_after;
            if let Some(ordinal) = after {
                assert!(!ordinals.contains(&ordinal));
                ordinals.push(ordinal);
            }
            entries.extend(page.items);
            if !page.has_more {
                break;
            }
        }
        let names: Vec<_> = entries
            .iter()
            .map(|e| Path::new(&e.path).file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["z.pdf", "same.pdf", "same.pdf", "a.pdf", "child.pdf", "zero.pdf"]
        );
        let mut twin_private: Vec<_> = entries[1..3].iter().map(|e| e.private_bytes).collect();
        twin_private.sort();
        assert_eq!(twin_private, [None, Some(9)]);
        assert_eq!(entries[4].private_bytes, None);
        assert_eq!(entries[5].size_bytes, 0);
        // Exact-collision tie order is the first visit, never an allocation or
        // an accidental heap ordering. Neither legacy Item nor find is changed.
        assert!(page_cmp(40, Path::new("same.pdf"), 1, 40, Path::new("same.pdf"), 2).is_gt());
    }

    #[test]
    fn category_pages_prune_exclude_root_and_reject_nonmatching_boundaries_before_candidates() {
        let cat = reg().find_by_name("node_modules").unwrap();
        let root = directory(
            "root",
            cat,
            true,
            vec![
                directory("a", cat, true, vec![directory("nested", cat, true, vec![])]),
                directory(
                    "ordinary",
                    reg().other(),
                    false,
                    vec![directory("b", cat, true, vec![])],
                ),
                file("inherited", 0, cat),
            ],
        );
        page_metrics::reset();
        let first = find_page(&root, Path::new("root"), cat, 1, SizeMode::Du, true, None, None).unwrap();
        assert_eq!(first.matched_items, 2);
        assert_eq!(page_metrics::get().visited_nodes, 4);
        assert_eq!(
            first.items[0].path,
            Path::new("root").join("ordinary").join("b").display().to_string()
        );
        let last = find_page(
            &root,
            Path::new("root"),
            cat,
            1,
            SizeMode::Du,
            true,
            None,
            first.next_after,
        )
        .unwrap();
        assert_eq!(last.matched_items, 2);
        assert_eq!(last.offset, 1);
        assert!(last.items_truncated);
        assert!(!last.has_more);
        assert_eq!(last.items[0].path, Path::new("root").join("a").display().to_string());
        // Visits are ordinary=1, b=2, inherited=3, a=4; the nested match
        // never gets an ordinal because its matching parent prunes it.
        for after in [0, 1, 3, 5, u64::MAX] {
            page_metrics::reset();
            let result = find_page(&root, Path::new("root"), cat, 1, SizeMode::Du, true, None, Some(after));
            assert!(matches!(result, Err(CategoryPageError::InvalidBoundary)), "{after}");
            let metrics = page_metrics::get();
            assert_eq!(metrics.walks, usize::from(after != 0));
            assert_eq!(metrics.candidate_paths, 0);
            assert_eq!(metrics.boundary_paths, 0);
            assert_eq!(metrics.max_output_entries, 0);
        }
        let terminal = find_page(&root, Path::new("root"), cat, 1, SizeMode::Du, true, None, Some(4)).unwrap();
        assert_eq!(terminal.offset, 2);
        assert_eq!(terminal.matched_items, 2);
        assert!(terminal.items.is_empty() && terminal.items_truncated && !terminal.has_more);
        assert!(terminal.next_after.is_none());
        let empty = find_page(
            &root,
            Path::new("root"),
            reg().find_by_name("PDF").unwrap(),
            10,
            SizeMode::Du,
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(empty.matched_items, 0);
        assert_eq!(empty.offset, 0);
        assert!(empty.items.is_empty() && !empty.items_truncated && !empty.has_more);
        assert!(empty.next_after.is_none());
    }

    #[test]
    fn category_pages_use_snapshot_dedup_hardlink_and_freeable_sizes() {
        use crate::scan::Shared;

        let cat = reg().find_by_name("PDF").unwrap();
        let mut cloned = file("clone.pdf", 100, cat);
        cloned.clone = true;
        let mut linked = file("link.pdf", 0, cat);
        linked.hardlink = true;
        let mut root = directory(
            "root",
            reg().other(),
            false,
            vec![cloned, file("owner.pdf", 50, cat), linked, file("zero.pdf", 0, cat)],
        );
        root.dir.as_mut().unwrap().clones_mut().shared = vec![Shared {
            idx: 0,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 2,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            for known in [false, true] {
                let full = find(&root, Path::new("root"), cat, 4, mode, known, None);
                let first = find_page(&root, Path::new("root"), cat, 2, mode, known, None, None).unwrap();
                let last = find_page(&root, Path::new("root"), cat, 2, mode, known, None, first.next_after).unwrap();
                assert_eq!(first.matched_items, 4);
                assert_eq!(last.matched_items, 4);
                assert_eq!(last.offset, 2);
                assert!(first.items_truncated && last.items_truncated);
                let mut joined = page_items(&first).as_array().unwrap().clone();
                joined.extend(page_items(&last).as_array().unwrap().iter().cloned());
                assert_eq!(
                    serde_json::Value::Array(joined),
                    serde_json::to_value(&full.items).unwrap()
                );
                assert_eq!(
                    Path::new(&first.items[0].path).file_name().unwrap(),
                    if mode == SizeMode::Du { "clone.pdf" } else { "owner.pdf" },
                );
                assert_eq!(first.items[0].freeable_min_bytes.is_some(), known);
            }
        }
    }

    #[test]
    fn mixed_query_pages_keep_snapshot_accounting_pruning_and_bounded_selection() {
        use crate::scan::Shared;

        let cat = reg().find_by_name("PDF").unwrap();
        // Model a custom directory+PDF category without mutating the global registry.
        // Integration tests exercise construction from an actual custom configuration.
        let query = CategoryQuery {
            cat,
            want_dirs: true,
            include_files: true,
        };
        let mut cloned = file("clone.pdf", 100, cat);
        cloned.clone = true;
        let mut linked = file("link.pdf", 0, cat);
        linked.hardlink = true;
        let mut root = directory(
            "root",
            cat,
            true,
            vec![
                cloned,
                file("owner.pdf", 50, cat),
                linked,
                file("zero.pdf", 0, cat),
                directory("special", cat, true, vec![file("pruned.pdf", 1, cat)]),
                file("inherited.asset", 1000, cat),
                file("wrong-category.pdf", 1000, reg().other()),
            ],
        );
        let clone_index = root
            .children()
            .iter()
            .position(|node| &*node.name == "clone.pdf")
            .unwrap();
        root.dir.as_mut().unwrap().clones_mut().shared = vec![Shared {
            idx: clone_index as u32,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 2,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        let mut private = PrivateSizes::default();
        private.0.insert(PathBuf::from("clone.pdf"), 9);
        private.0.insert(PathBuf::from("owner.pdf"), 7);
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            for known in [false, true] {
                let full = find(&root, Path::new("root"), query, 5, mode, known, Some(&private));
                assert_eq!(full.matched_items, 5);
                assert!(!full.items_truncated);
                assert_eq!(
                    Path::new(&full.items[0].path).file_name().unwrap(),
                    if mode == SizeMode::Du { "clone.pdf" } else { "owner.pdf" }
                );
                assert_eq!(full.items[0].freeable_min_bytes.is_some(), known);
                assert!(full.items.iter().all(|item| !item.path.contains("pruned")
                    && !item.path.contains("inherited")
                    && !item.path.contains("wrong-category")));
                let clone = full.items.iter().find(|item| item.path.ends_with("clone.pdf")).unwrap();
                assert_eq!(clone.size_bytes, 100);
                assert_eq!(clone.dedup_bytes, 0);
                assert_eq!(clone.private_bytes, Some(9));
                assert!(
                    full.items
                        .iter()
                        .find(|item| item.path.ends_with("special"))
                        .unwrap()
                        .private_bytes
                        .is_none()
                );
                for top in [0, 1, 2, 5] {
                    let limited = find(&root, Path::new("root"), query, top, mode, known, Some(&private));
                    assert_eq!(limited.matched_items, 5);
                    assert_eq!(limited.items_truncated, top < 5);
                    assert_eq!(
                        serde_json::to_value(&limited.items).unwrap(),
                        serde_json::to_value(&full.items[..top]).unwrap()
                    );
                }
                let mut after = None;
                let mut joined = Vec::new();
                loop {
                    page_metrics::reset();
                    let page =
                        find_page(&root, Path::new("root"), query, 2, mode, known, Some(&private), after).unwrap();
                    assert_page_bounds(2, if after.is_some() { 2 } else { 1 });
                    assert_eq!(page.matched_items, 5);
                    assert_eq!(page.offset as usize, joined.len());
                    joined.extend(page_items(&page).as_array().unwrap().iter().cloned());
                    after = page.next_after;
                    if after.is_none() {
                        break;
                    }
                }
                assert_eq!(
                    serde_json::Value::Array(joined),
                    serde_json::to_value(&full.items).unwrap()
                );
            }
        }
    }

    #[test]
    fn category_page_size_and_counter_arithmetic_are_checked() {
        let cat = reg().find_by_name("PDF").unwrap();
        let root = directory("root", reg().other(), false, vec![file("file.pdf", 1, cat)]);
        for top in [0, 501, usize::MAX] {
            page_metrics::reset();
            let result = find_page(&root, Path::new("root"), cat, top, SizeMode::Du, true, None, None);
            assert!(matches!(result, Err(CategoryPageError::InvalidTop)));
            assert_eq!(page_metrics::get().walks, 0);
            assert_eq!(page_metrics::get().candidate_paths, 0);
        }
        let mut value = u64::MAX - 1;
        assert_eq!(page_increment(&mut value), Ok(()));
        assert_eq!(value, u64::MAX);
        assert_eq!(page_increment(&mut value), Err(CategoryPageError::CountOverflow));
        assert_eq!(value, u64::MAX);
        let page = find_page(&root, Path::new("root"), cat, 500, SizeMode::Du, true, None, None).unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(!page.items_truncated && !page.has_more && page.next_after.is_none());
    }

    #[test]
    fn category_page_work_counters() {
        let cat = reg().find_by_name("PDF").unwrap();
        for count in [600, 20_000] {
            let root = directory(
                "root",
                reg().other(),
                false,
                (0..count).map(|i| file(&format!("f{i:05}.pdf"), 1, cat)).collect(),
            );
            page_metrics::reset();
            let first = find_page(&root, Path::new("root"), cat, 50, SizeMode::Du, true, None, None).unwrap();
            let first_metrics = page_metrics::get();
            assert_page_bounds(50, 1);
            assert_eq!(first_metrics.visited_nodes, count);
            assert_eq!(first_metrics.candidate_paths, 50);
            assert_eq!(first_metrics.boundary_paths, 0);
            assert_eq!(first_metrics.max_live_candidates, 50);
            assert_eq!(first_metrics.max_output_entries, 50);
            assert_eq!(first.offset, 0);
            assert_eq!(first.matched_items, count as u64);
            assert_eq!(first.items.len(), 50);
            assert!(first.has_more);

            // Advance using legal bounded pages, never an all-results or
            // offset-sized helper. Keep only the cursor between calls.
            let target = count - 50;
            let mut offset = 0;
            let mut after = None;
            while offset < target {
                let step = (target - offset).min(500);
                let page = find_page(&root, Path::new("root"), cat, step, SizeMode::Du, true, None, after).unwrap();
                assert_eq!(page.offset, offset as u64);
                assert_eq!(page.items.len(), step);
                offset += step;
                after = page.next_after;
                assert!(after.is_some());
            }
            page_metrics::reset();
            let late = find_page(&root, Path::new("root"), cat, 50, SizeMode::Du, true, None, after).unwrap();
            let late_metrics = page_metrics::get();
            assert_page_bounds(50, 2);
            assert_eq!(late_metrics.visited_nodes, 2 * count);
            assert_eq!(late_metrics.candidate_paths, 50);
            assert_eq!(late_metrics.boundary_paths, 1);
            assert_eq!(late_metrics.max_live_candidates, 50);
            assert_eq!(late_metrics.max_output_entries, 50);
            assert_eq!(late.offset, target as u64);
            assert_eq!(late.matched_items, count as u64);
            assert_eq!(late.items.len(), 50);
            assert!(!late.has_more);
            assert!(late.items_truncated);
            for (phase, offset, metrics) in [("first", 0, first_metrics), ("late", target, late_metrics)] {
                println!(
                    "pagination_work matches={count} phase={phase} offset={offset} top=50 walks={} visited_nodes={} candidate_path_constructions={} boundary_path_constructions={} max_live_candidates={} max_output_entries={}",
                    metrics.walks,
                    metrics.visited_nodes,
                    metrics.candidate_paths,
                    metrics.boundary_paths,
                    metrics.max_live_candidates,
                    metrics.max_output_entries,
                );
            }
        }
    }

    #[test]
    fn category_pages_handle_deep_paths_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let cat = reg().find_by_name("PDF").unwrap();
                let mut root = directory(
                    "leaf",
                    reg().other(),
                    false,
                    vec![file("a.pdf", 0, cat), file("z.pdf", 0, cat)],
                );
                for _ in 0..3000 {
                    root = directory("d", reg().other(), false, vec![root]);
                }
                page_metrics::reset();
                let first = find_page(&root, Path::new("root"), cat, 1, SizeMode::Du, true, None, None).unwrap();
                assert_page_bounds(1, 1);
                assert!(first.items[0].path.ends_with("z.pdf"));
                page_metrics::reset();
                let last = find_page(
                    &root,
                    Path::new("root"),
                    cat,
                    1,
                    SizeMode::Du,
                    true,
                    None,
                    first.next_after,
                )
                .unwrap();
                assert_page_bounds(1, 2);
                assert_eq!(last.matched_items, 2);
                assert_eq!(last.offset, 1);
                assert!(last.items[0].path.ends_with("a.pdf"));
                assert!(last.items_truncated && !last.has_more);
                drop(root);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn category_and_full_text_share_scan_notices_with_exact_placement() {
        let root = Node {
            name: "root".into(),
            size: 0,
            cat: reg().other(),
            special: false,
            hardlink: false,
            lossy: false,
            clone: false,
            dir: Some(Box::new(crate::scan::DirInfo {
                cats: vec![0; reg().len()].into_boxed_slice(),
                files: 0,
                errors: 0,
                children: Vec::new(),
                free_base: 0,
                clones: None,
            })),
        };
        // Future timestamps use a stable "just now" label, independent of
        // the test runner's clock or a minute boundary during the assertion.
        let cached = UNIX_EPOCH + Duration::from_secs(7_258_118_400);
        for (cached_at, unreadable, notices) in [
            (None, 0, ""),
            (None, 7, "(7 entries unreadable)\n"),
            (Some(cached), 0, "(from cache, scanned just now; --refresh to rescan)\n"),
            (
                Some(cached),
                7,
                "(from cache, scanned just now; --refresh to rescan)\n(7 entries unreadable)\n",
            ),
        ] {
            let meta = Meta {
                cached_at,
                unreadable,
                ..plain_meta()
            };
            let cat = reg().find_by_name("PDF").unwrap();
            let items = find(&root, Path::new("root"), cat, 20, SizeMode::Du, true, None);
            assert!(items.items.is_empty());
            assert_eq!(items.matched_items, 0);
            assert!(!items.items_truncated);
            assert_eq!(
                find_to_text(cat, &items, &meta),
                format!("{notices}Largest PDF (0 of 0 shown, 0 B displayed total):\n")
            );
            let report = build(&root, Path::new("root"), &meta, 20);
            assert!(to_text(&report, &root).starts_with(&format!(
                "root  —  0 B  ·  0 files  ·  0.00s\n{notices}\nBy category:\n"
            )));
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["unreadable"].as_u64(), Some(unreadable));
            assert_eq!(json["cached_at"], serde_json::json!(unix_secs(cached_at)));
        }
    }

    #[test]
    fn top_n_ties_use_the_same_path_order_as_sorting() {
        let root_path = std::env::temp_dir().join(format!("dutype-report-ties-{}", std::process::id()));
        std::fs::create_dir(&root_path).unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root_path.join(name), []).unwrap();
        }
        let root =
            crate::scan::scan_excluding(&root_path, true, false, &crate::scan::Progress::default(), None).unwrap();
        let meta = Meta {
            elapsed: Duration::ZERO,
            cached_at: None,
            unreadable: 0,
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            shares: false,
            size_mode: SizeMode::Du,
            private: None,
            reflink_fs: None,
        };
        let report = build(&root, &root_path, &meta, 2);
        let cat = reg().classify_file("a.rs");
        let found = find(&root, &root_path, cat, 2, SizeMode::Du, true, None);
        std::fs::remove_dir_all(&root_path).unwrap();
        let names = |items: &[Entry]| {
            items
                .iter()
                .map(|e| Path::new(&e.path).file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&report.largest_files), ["c.rs", "b.rs"]);
        assert_eq!(names(&found.items), ["c.rs", "b.rs"]);
        assert_eq!(found.matched_items, 3);
        assert!(found.items_truncated);
    }

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
            reflink_fs: None,
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
                reflink_fs: None,
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

    /// No freeable bound where files may share blocks unless they were
    /// detected (APFS clone flags, or shared extents read with `--clones`);
    /// shared extents are reported as `fiemap`.
    #[test]
    fn freeable_needs_detection_where_blocks_may_be_shared() {
        assert!(detected(true, false, Some(ReflinkFs::Btrfs)) && detected(true, true, None));
        assert!(!detected(true, false, None) && !detected(false, true, None));
        assert!(!freeable_known(false, true) && freeable_known(false, false) && freeable_known(true, true));
        let meta = |clones, reflink_fs| Meta {
            elapsed: Duration::ZERO,
            cached_at: None,
            unreadable: 0,
            clones,
            clone_ids: false,
            clone_gaps: false,
            shares: true,
            reflink_fs,
            size_mode: SizeMode::Du,
            private: None,
        };
        let a = accounting(&meta(true, Some(ReflinkFs::Btrfs)));
        assert_eq!(a.clone_detection, "fiemap");
        assert!(a.caveats.contains(&"snapshots-counted-as-shared"));
        let a = accounting(&meta(true, Some(ReflinkFs::Xfs)));
        assert_eq!(
            (a.clone_detection, a.caveats),
            ("fiemap", vec!["shared-extents-not-freeable"])
        );
        assert_eq!(accounting(&meta(false, Some(ReflinkFs::Xfs))).clone_detection, "off");
        assert_eq!(accounting(&meta(true, None)).clone_detection, "unsupported");
        let gaps = Meta {
            clone_gaps: true,
            ..meta(true, Some(ReflinkFs::Xfs))
        };
        let a = accounting(&gaps);
        assert_eq!(a.clone_detection, "fiemap-partial");
        assert!(a.caveats.contains(&"shared-extents-missing-for-some-files"));
    }

    #[test]
    fn shared_blocks_are_labelled_by_detection() {
        for (state, label) in [
            ("fiemap", "shared extents"),
            ("fiemap-partial", "shared extents"),
            ("apfs-full", "APFS clones"),
            ("apfs-partial", "APFS clones"),
        ] {
            assert_eq!(shared_label(state), label, "{state}");
        }
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
        let items = CategoryMatches {
            matched_items: 4,
            items_truncated: true,
            items: vec![item("/a", 3 << 20, 3 << 20), item("/b", 2 << 20, 0)],
        };
        let du = find_to_text(reg().other(), &items, &plain_meta());
        assert!(
            du.contains("2 of 4 shown, 5.0 MiB displayed total)") && du.contains("2.0 MiB  /b"),
            "{du}"
        );
        let dedup = find_to_text(
            reg().other(),
            &items,
            &Meta {
                size_mode: SizeMode::Dedup,
                ..plain_meta()
            },
        );
        assert!(
            dedup.contains("2 of 4 shown, 3.0 MiB displayed total, clones counted once)"),
            "{dedup}"
        );
        assert!(dedup.contains("0 B  /b"), "{dedup}");
        let warned = find_to_text(
            reg().other(),
            &items,
            &Meta {
                unreadable: 2,
                size_mode: SizeMode::Dedup,
                ..plain_meta()
            },
        );
        assert_eq!(warned, format!("(2 entries unreadable)\n{dedup}"));
    }
    #[test]
    fn human_reports_escape_paths_without_changing_json_payloads() {
        let pdf = reg().classify_file("a.pdf");
        let special = reg().classify_file("a.txt");
        let names = [
            "a\nb.pdf",
            // Backslash is a separator on Windows, never part of one scanned
            // node name. Keep traversal fixtures within that native invariant.
            #[cfg(not(windows))]
            "a\\nb.pdf",
            "ab.pdf",
            "a\tb.pdf",
            "a\rb.pdf",
            "a\x1bb.pdf",
            "a\u{0085}b.pdf",
            "a\u{202e}b.pdf",
            "a\u{2028}b.pdf",
            "-leading.pdf",
            "résumé-👩\u{200d}💻.pdf",
        ];
        for name in names {
            assert_eq!(Path::new(name).components().count(), 1, "fixture name {name:?}");
        }
        let children = names
            .iter()
            .map(|name| file(name, 4096, pdf))
            .chain([directory("special\n", special, true, vec![])])
            .collect();
        let root = directory("root\n", reg().other(), false, children);
        let path = Path::new("/root\n");
        let meta = plain_meta();
        let report = build(&root, path, &meta, 100);
        let before = serde_json::to_vec(&report).unwrap();
        let text = to_text(&report, &root);
        assert!(text.starts_with("/root\\n  —"));
        assert!(text.contains("a\\nb.pdf"));
        #[cfg(not(windows))]
        assert!(text.contains("a\\\\nb.pdf"));
        assert!(text.contains("a\\x1bb.pdf"));
        assert!(text.contains("a\\u{0085}b.pdf"));
        assert!(text.contains("a\\u{202e}b.pdf"));
        assert!(text.contains("a\\u{2028}b.pdf"));
        assert!(text.contains("special\\n/"));
        assert!(text.contains(display::path(&path.join("special\n")).as_ref()));
        assert!(text.contains("résumé-👩\u{200d}💻.pdf"));
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
        assert_eq!(serde_json::to_vec(&report).unwrap(), before);
        assert_eq!(report.path, "/root\n");
        for name in names {
            assert!(report.children.iter().any(|child| child.name == name));
        }
        let found = find(&root, path, pdf, 100, SizeMode::Du, true, None);
        let before = serde_json::to_vec(&found.items).unwrap();
        let text = find_to_text(pdf, &found, &meta);
        assert_eq!(text.lines().count(), names.len() + 1);
        assert!(text.contains(display::path(&path.join("a\nb.pdf")).as_ref()));
        assert_eq!(serde_json::to_vec(&found.items).unwrap(), before);
    }
    #[test]
    fn report_component_collision_encoding_does_not_depend_on_native_path_parsing() {
        let pdf = reg().classify_file("a.pdf");
        let root = directory(
            "root",
            reg().other(),
            false,
            vec![file("a\nb.pdf", 4096, pdf), file("placeholder.pdf", 4096, pdf)],
        );
        let mut report = build(&root, Path::new("root"), &plain_meta(), 100);
        // Test the raw report-name rendering boundary on every platform without
        // pushing an impossible Windows backslash component through a tree walk.
        report
            .children
            .iter_mut()
            .find(|child| child.name == "placeholder.pdf")
            .unwrap()
            .name = "a\\nb.pdf".into();
        let before = serde_json::to_vec(&report).unwrap();
        let text = to_text(&report, &root);
        assert!(text.contains("a\\nb.pdf"));
        assert!(text.contains("a\\\\nb.pdf"));
        assert_eq!(serde_json::to_vec(&report).unwrap(), before);
        assert!(report.children.iter().any(|child| child.name == "a\nb.pdf"));
        assert!(report.children.iter().any(|child| child.name == "a\\nb.pdf"));
    }
}
