use crate::category::{self, Category, by_size, reg};
use crate::report::SizeMode;
use crate::scan::{Node, TopN, walk};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const POPUP_LIMIT: usize = 500;
const EXT_LIMIT: usize = 60;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Entries,
    Breakdown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Category,
    Extension,
}

#[derive(Clone)]
pub enum Key {
    Cat(Category),
    Ext(String),
}

pub struct BreakdownItem {
    pub key: Key,
    pub label: String,
    /// Category used for color and icon (for extensions: what they classify as).
    pub cat: Category,
    pub size: u64,
    pub count: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rescan {
    /// `r`: rescan the current folder and splice it into the tree.
    Current,
    /// `R`: rescan everything from the root.
    All,
}

/// One folder of a location. The name alone is not an identity: an escaped
/// non-UTF-8 name (e.g. `d\xffir`) can be spelled exactly like a sibling's
/// real UTF-8 name, so lossiness is part of the match. On unix escapes are
/// reversible, so two different non-UTF-8 names never share a Step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: Box<str>,
    pub lossy: bool,
}

impl Step {
    fn of(n: &Node) -> Self {
        Step {
            name: n.name.clone(),
            lossy: n.lossy,
        }
    }

    fn matches(&self, n: &Node) -> bool {
        n.lossy == self.lossy && n.name == self.name
    }
}

impl From<&str> for Step {
    fn from(name: &str) -> Self {
        Step {
            name: name.into(),
            lossy: false,
        }
    }
}

pub struct PopupItem {
    /// Relative to the folder the popup was opened in; display only.
    pub path: PathBuf,
    pub size: u64,
    /// Real path for reveal/copy, unavailable for ambiguous non-UTF-8 names.
    pub fs_path: Option<PathBuf>,
    /// Child indices from the current folder in this snapshot. Never resolve
    /// the display path: distinct raw names can render identically.
    target: Vec<usize>,
}

impl PopupItem {
    /// Match report size/path ranking. Exact display collisions prefer the
    /// earlier snapshot visit; raw filesystem paths are payload, not rank.
    fn cmp_rank(&self, size: u64, path: &Path, target: &[usize]) -> Ordering {
        self.size
            .cmp(&size)
            .then_with(|| self.path.as_path().cmp(path))
            .then_with(|| target.cmp(&self.target))
    }
}

impl PartialEq for PopupItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for PopupItem {}

impl PartialOrd for PopupItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PopupItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_rank(other.size, &other.path, &other.target)
    }
}

pub struct Popup {
    pub title: String,
    pub items: Vec<PopupItem>,
    pub state: TableState,
}

pub struct App {
    pub root: Node,
    pub root_path: PathBuf,
    /// Child indices from root to the current directory.
    pub stack: Vec<usize>,
    /// Selection of the entry list, one per level of `stack` (+1 for current).
    pub states: Vec<TableState>,
    pub focus: Focus,
    pub mode: Mode,
    pub breakdown: Vec<BreakdownItem>,
    pub breakdown_state: TableState,
    pub popup: Option<Popup>,
    pub scan_time: Duration,
    /// When the shown scan ran, if it came from the `--cache`.
    pub cached_at: Option<SystemTime>,
    pub errors: u64,
    pub quit: bool,
    /// Set by `r` / `R`; the caller performs the rescan. `All` also quits the
    /// event loop so a fresh `App` is built.
    pub rescan: Option<Rescan>,
    /// One-shot message in the footer (e.g. after copying a path).
    pub status: Option<String>,
    /// Screencast mode: show the last key pressed (for demos and recordings).
    pub show_keys: bool,
    /// Device of the scan root when the tree was built (None: unknown).
    pub root_dev: Option<u64>,
    /// The scan looked for APFS clones (`--clones`).
    pub clones: bool,
    /// The scan could see APFS clone families.
    pub clone_ids: bool,
    /// ... but some files' clone flags could not be read.
    pub clone_gaps: bool,
    /// Files may share blocks on the scanned filesystem (see
    /// [`crate::scan::may_share_blocks`]).
    pub shares: bool,
    /// Scans collect APFS private sizes (`--private`), rescans included.
    pub private: bool,
    /// Partial clones could be told apart (see `Snapshot::private_sizes`).
    pub private_sizes: bool,
    /// ... but some private size was not reported: none are shown.
    pub private_missing: bool,
    /// The private sizes (see [`crate::scan::PrivateSizes`]).
    pub privates: crate::scan::PrivateSizes,
    /// The scan root is on a Linux reflink filesystem (whose shared extents
    /// `--clones` reads).
    pub reflink_fs: Option<crate::scan::ReflinkFs>,
    /// Size shown and sorted by (`c` toggles).
    pub size_mode: SizeMode,
    pub last_key: Option<(String, &'static str)>,
}

pub fn node_at<'a>(root: &'a Node, stack: &[usize]) -> &'a Node {
    stack.iter().fold(root, |n, &i| &n.children()[i])
}

fn node_at_mut<'a>(root: &'a mut Node, stack: &[usize]) -> &'a mut Node {
    stack.iter().fold(root, |n, &i| {
        &mut n.dir.as_mut().expect("stack points at a directory").children[i]
    })
}

fn first_selected(len: usize) -> TableState {
    TableState::default().with_selected((len > 0).then_some(0))
}

impl App {
    pub fn new(root: Node, root_path: PathBuf, scan_time: Duration, errors: u64) -> Self {
        // root_dev is set by the caller, which knows when the tree was built.
        let state = first_selected(root.children().len());
        let mut app = App {
            root,
            root_path,
            stack: Vec::new(),
            states: vec![state],
            focus: Focus::Entries,
            mode: Mode::Category,
            breakdown: Vec::new(),
            breakdown_state: TableState::default(),
            popup: None,
            scan_time,
            cached_at: None,
            errors,
            quit: false,
            rescan: None,
            status: None,
            show_keys: false,
            root_dev: None,
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            shares: true,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: Default::default(),
            reflink_fs: None,
            size_mode: SizeMode::Du,
            last_key: None,
        };
        app.refresh_breakdown();
        app
    }

    pub fn current(&self) -> &Node {
        node_at(&self.root, &self.stack)
    }

    pub fn current_path(&self) -> PathBuf {
        let mut p = self.root_path.clone();
        let mut n = &self.root;
        for &i in &self.stack {
            n = &n.children()[i];
            p.push(&*n.name);
        }
        p
    }

    /// The current folder relative to the root, by the tree's names (as
    /// [`crate::scan::PrivateSizes`] keys it).
    pub fn current_rel(&self) -> PathBuf {
        let mut p = PathBuf::new();
        let mut n = &self.root;
        for &i in &self.stack {
            n = &n.children()[i];
            p.push(&*n.name);
        }
        p
    }

    /// Take in the private sizes of a rescan of the current folder. Their
    /// coverage of the whole tree: a rescan of the root replaces it, one of
    /// a folder only adds to what was seen or missed.
    pub fn merge_private(&mut self, sizes: bool, missing: bool, fresh: crate::scan::PrivateSizes) {
        let below = self.current_rel();
        if self.stack.is_empty() {
            (self.private_sizes, self.private_missing) = (sizes, missing);
        } else {
            self.private_sizes |= sizes;
            self.private_missing |= missing;
        }
        self.privates.replace_below(&below, fresh);
    }

    /// The current folder's real path on disk, for filesystem operations
    /// (rescan, copy, reveal); [`Self::current_path`] is for display.
    /// `None` if a non-UTF-8 name on the way cannot be resolved uniquely.
    pub fn fs_path(&self) -> Option<PathBuf> {
        let mut p = self.root_path.clone();
        let mut n = &self.root;
        for &i in &self.stack {
            n = &n.children()[i];
            p = crate::scan::resolve_child(&p, n)?;
        }
        Some(p)
    }

    /// Like [`Self::fs_path`], but `None` unless every folder from the root
    /// down is still a real directory: a component replaced by a symlink
    /// would make a subtree scan follow it, unlike a full scan.
    pub fn fs_path_checked(&self) -> Option<PathBuf> {
        let mut p = self.root_path.clone();
        let mut n = &self.root;
        for &i in &self.stack {
            n = &n.children()[i];
            p = crate::scan::resolve_child(&p, n)?;
            if !crate::scan::is_real_dir(&p) {
                return None;
            }
        }
        Some(p)
    }

    /// Real path of child `i` of the current folder.
    fn child_fs_path(&self, i: usize) -> Option<PathBuf> {
        crate::scan::resolve_child(&self.fs_path()?, &self.current().children()[i])
    }

    /// Some folder from the root to here has a non-UTF-8 name. Its display
    /// name may collide with a sibling, so `r` rescans everything instead.
    pub fn path_has_lossy_name(&self) -> bool {
        let mut n = &self.root;
        self.stack.iter().any(|&i| {
            n = &n.children()[i];
            n.lossy
        })
    }

    /// Folders from the root to the current directory.
    pub fn location(&self) -> Vec<Step> {
        let mut n = &self.root;
        self.stack
            .iter()
            .map(|&i| {
                n = &n.children()[i];
                Step::of(n)
            })
            .collect()
    }

    /// Descend into the directories of `location` (as far as they exist).
    pub fn go_to(&mut self, location: &[Step]) {
        for step in location {
            let Some(i) = self
                .current()
                .children()
                .iter()
                .position(|c| c.is_dir() && step.matches(c))
            else {
                break;
            };
            self.states.last_mut().unwrap().select(Some(i));
            self.enter();
        }
    }

    /// Special category the current folder inherits, for
    /// [`crate::scan::scan_subtree`]: that of the nearest special ancestor.
    /// (A dir's `cat` cannot tell "inherits Other" from "inherits nothing".)
    pub fn inherited_category(&self) -> Option<Category> {
        (0..self.stack.len())
            .rev()
            .map(|k| node_at(&self.root, &self.stack[..k]))
            .find(|n| n.special)
            .map(|n| n.cat)
    }

    /// Replace the current folder with a fresh scan of it, update every
    /// ancestor's totals and order, and keep the cursor on the same entries.
    /// `clone_ids`: the rescan saw APFS clone flags (see
    /// `Progress::clone_ids`).
    pub fn replace_current(&mut self, mut fresh: Node, clone_ids: bool) {
        // A rescan of the root replaces what the tree saw; below it adds.
        match self.stack.is_empty() {
            true => self.clone_ids = clone_ids,
            false => self.clone_ids |= clone_ids,
        }
        let place = self.remember();

        let target = node_at_mut(&mut self.root, &self.stack);
        fresh.name = target.name.clone();
        let old = std::mem::replace(target, fresh);
        let new = node_at(&self.root, &self.stack);
        let size_delta = new.size as i128 - old.size as i128;
        let files_delta = new.files() as i128 - old.files() as i128;
        let errors_delta = new.errors() as i128 - old.errors() as i128;
        let gaps_delta = new.clone_gaps() as i128 - old.clone_gaps() as i128;
        let free_delta =
            new.dir.as_ref().map_or(0, |d| d.free_base) as i128 - old.dir.as_ref().map_or(0, |d| d.free_base) as i128;
        let cats_delta: Vec<i128> = new
            .cats()
            .iter()
            .zip(old.cats().iter())
            .map(|(n, o)| *n as i128 - *o as i128)
            .collect();
        let shift = |v: &mut u64, d: i128| *v = (*v as i128 + d).max(0) as u64;
        for k in (0..self.stack.len()).rev() {
            let node = node_at_mut(&mut self.root, &self.stack[..k]);
            shift(&mut node.size, size_delta);
            if let Some(dir) = node.dir.as_mut() {
                shift(&mut dir.files, files_delta);
                shift(&mut dir.errors, errors_delta);
                if gaps_delta != 0 {
                    shift(&mut dir.clones_mut().gaps, gaps_delta);
                    dir.trim_clones();
                }
                shift(&mut dir.free_base, free_delta);
                dir.cats.iter_mut().zip(&cats_delta).for_each(|(c, d)| shift(c, *d));
                // Clones may have appeared or gone below.
                node.clone = dir.children.iter().any(|c| c.clone);
            }
        }
        // Clone families may span the old and new subtree and the rest.
        if self.clones {
            crate::account::account(&mut self.root, Some(&self.root_path));
        }
        let by_dedup = self.size_mode == SizeMode::Dedup;
        if by_dedup {
            sort_tree(&mut self.root, true);
        }
        // Ancestors' sizes changed; `sort_tree` skips those left without
        // clones below.
        for k in (0..self.stack.len()).rev() {
            if let Some(dir) = node_at_mut(&mut self.root, &self.stack[..k]).dir.as_mut() {
                dir.sort_children(by_dedup);
            }
        }
        // The subtree's old errors and clone gaps were replaced, not added to.
        self.errors = self.root.errors();
        self.clone_gaps = self.root.clone_gaps() > 0;
        // Free the old subtree off the UI thread; it can be large.
        std::thread::spawn(move || drop(old));
        self.restore(place);
    }

    /// The location and the selected entry at every level, by name.
    fn remember(&self) -> (Vec<Step>, Vec<Option<Step>>) {
        let selected = (0..=self.stack.len())
            .map(|k| {
                let node = node_at(&self.root, &self.stack[..k]);
                self.states[k]
                    .selected()
                    .and_then(|i| node.children().get(i))
                    .map(Step::of)
            })
            .collect();
        (self.location(), selected)
    }

    /// Back to a [`Self::remember`]ed place after the tree was re-sorted.
    fn restore(&mut self, (location, selected): (Vec<Step>, Vec<Option<Step>>)) {
        self.stack.clear();
        self.states = vec![first_selected(self.root.children().len())];
        self.go_to(&location);
        for (k, name) in selected.iter().enumerate().take(self.states.len()) {
            let node = node_at(&self.root, &self.stack[..k]);
            let pos = name
                .as_ref()
                .and_then(|step| node.children().iter().position(|c| step.matches(c)));
            self.states[k].select(pos.or(if node.children().is_empty() { None } else { Some(0) }));
        }
        self.popup = None;
        self.refresh_breakdown();
    }

    /// Show and sort by `mode`, keeping the cursor on the same entries.
    pub fn set_size_mode(&mut self, mode: SizeMode) {
        if mode == self.size_mode {
            return;
        }
        let place = self.remember();
        self.size_mode = mode;
        sort_tree(&mut self.root, mode == SizeMode::Dedup);
        self.restore(place);
    }

    /// Run `f` on the tree in du order, the order scans give and caches
    /// store, and go back to the current order afterwards.
    pub fn in_du_order<T>(&mut self, f: impl FnOnce(&mut Node) -> T) -> T {
        // The callback gets mutable tree access, including in du mode.
        self.popup = None;
        if self.size_mode == SizeMode::Du {
            return f(&mut self.root);
        }
        let place = self.remember();
        sort_tree(&mut self.root, false);
        let out = f(&mut self.root);
        sort_tree(&mut self.root, true);
        self.restore(place);
        out
    }

    /// Lower bound of what deleting `n` frees, if it can be known (see
    /// [`crate::report::freeable_known`]).
    pub fn freeable(&self, n: &Node) -> Option<u64> {
        crate::report::freeable_of(
            n,
            crate::report::freeable_known(
                crate::report::detected(self.clones, self.clone_ids, self.reflink_fs),
                self.shares,
            ),
        )
    }

    /// Size of child `i` of the current folder in the current mode.
    pub fn shown_size(&self, dir: &Node, i: usize) -> u64 {
        match (self.size_mode, &dir.dir) {
            (SizeMode::Dedup, Some(d)) => d.child_dedup(i),
            _ => dir.children()[i].size,
        }
    }

    /// Size of a folder itself in the current mode.
    pub fn shown_total(&self, n: &Node) -> u64 {
        match self.size_mode {
            SizeMode::Du => n.size,
            SizeMode::Dedup => n.dedup(),
        }
    }

    fn selected_entry(&self) -> Option<usize> {
        self.states.last().and_then(|s| s.selected())
    }

    pub fn refresh_breakdown(&mut self) {
        let cur = node_at(&self.root, &self.stack);
        self.breakdown = match self.mode {
            Mode::Category => {
                let cats = cur.cats();
                by_size(&cats)
                    .into_iter()
                    .map(|c| BreakdownItem {
                        key: Key::Cat(c),
                        label: c.label().to_string(),
                        cat: c,
                        size: cats[c.index()],
                        count: None,
                    })
                    .collect()
            }
            Mode::Extension => ext_breakdown(cur),
        };
        self.breakdown_state = first_selected(self.breakdown.len());
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.show_keys {
            self.last_key = Some(self.describe_key(key));
        }
        self.status = None;
        if self.popup.is_some() {
            self.handle_popup_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-15),
            KeyCode::PageDown => self.move_selection(15),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.focus {
                Focus::Entries => self.enter(),
                Focus::Breakdown => self.open_popup(),
            },
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.leave(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Entries => Focus::Breakdown,
                    Focus::Breakdown => Focus::Entries,
                }
            }
            KeyCode::Char('c') if !self.clones => {
                self.status = Some("restart with --clones to count clones (APFS) or reflinks (btrfs, XFS) once".into());
            }
            KeyCode::Char('c') => {
                self.set_size_mode(match self.size_mode {
                    SizeMode::Du => SizeMode::Dedup,
                    SizeMode::Dedup => SizeMode::Du,
                });
                self.status = Some(match (self.size_mode, self.clone_ids, self.reflink_fs) {
                    (SizeMode::Dedup, _, Some(fs)) => format!("sizes: shared {} extents counted once", fs.label()),
                    (SizeMode::Dedup, true, None) => "sizes: APFS clones counted once".into(),
                    (SizeMode::Dedup, false, None) => "sizes: clones counted once (none detectable here)".into(),
                    (SizeMode::Du, ..) => "sizes: allocated, like du".into(),
                });
            }
            KeyCode::Char('m') => {
                self.mode = match self.mode {
                    Mode::Category => Mode::Extension,
                    Mode::Extension => Mode::Category,
                };
                self.refresh_breakdown();
            }
            KeyCode::Char('o') => {
                if let Some(i) = self.selected_entry() {
                    match self.child_fs_path(i) {
                        Some(path) => reveal(&path),
                        None => self.status = Some("cannot resolve this non-UTF-8 name".into()),
                    }
                }
            }
            KeyCode::Char('y') => {
                // The selected entry, or the current folder from the breakdown pane.
                let path = match (self.focus, self.selected_entry()) {
                    (Focus::Entries, Some(i)) => self.child_fs_path(i),
                    _ => self.fs_path(),
                };
                match path {
                    Some(path) => self.copy(&path),
                    None => self.status = Some("cannot resolve this non-UTF-8 name".into()),
                }
            }
            // At the root, rescanning "here" is a full rescan.
            KeyCode::Char('r') if !self.stack.is_empty() => self.rescan = Some(Rescan::Current),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.rescan = Some(Rescan::All);
                self.quit = true;
            }
            _ => {}
        }
    }

    fn copy(&mut self, path: &Path) {
        let shown = crate::ui::display_path(path);
        // The clipboard takes text; a lossy rendering would name another file.
        let Some(text) = path.to_str() else {
            self.status = Some(format!("cannot copy {shown}: the path is not valid UTF-8"));
            return;
        };
        self.status = Some(match copy_to_clipboard(text) {
            Ok(()) => format!("copied {shown}"),
            Err(e) => format!("copy failed: {e}"),
        });
    }

    /// Key glyph and what it does in the current state.
    fn describe_key(&self, key: KeyEvent) -> (String, &'static str) {
        let popup = self.popup.is_some();
        let (glyph, action) = match key.code {
            KeyCode::Up | KeyCode::Char('k') => ("↑".to_string(), "move"),
            KeyCode::Down | KeyCode::Char('j') => ("↓".to_string(), "move"),
            KeyCode::PageUp => ("PgUp".to_string(), "move"),
            KeyCode::PageDown => ("PgDn".to_string(), "move"),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let glyph = if key.code == KeyCode::Enter { "⏎ Enter" } else { "→" };
                let action = match (popup, self.focus) {
                    (true, _) => "reveal",
                    (false, Focus::Entries) => "open folder",
                    (false, Focus::Breakdown) => "list largest",
                };
                (glyph.to_string(), action)
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                ("←".to_string(), if popup { "close" } else { "up" })
            }
            KeyCode::Esc => ("Esc".to_string(), if popup { "close" } else { "quit" }),
            KeyCode::Tab | KeyCode::BackTab => ("⇥ Tab".to_string(), "switch pane"),
            KeyCode::Char('m') => ("m".to_string(), "category / extension"),
            KeyCode::Char('c') => ("c".to_string(), "du / dedup size"),
            KeyCode::Char('s') if popup => ("s".to_string(), "jump in TUI"),
            KeyCode::Char('o') => ("o".to_string(), "reveal"),
            KeyCode::Char('y') => ("y".to_string(), "copy path"),
            KeyCode::Char('r') if !popup => ("r".to_string(), "rescan folder"),
            KeyCode::Char('R') if !popup => ("R".to_string(), "rescan all"),
            KeyCode::Char('q') => ("q".to_string(), if popup { "close" } else { "quit" }),
            KeyCode::Char(c) => (c.to_string(), ""),
            _ => ("?".to_string(), ""),
        };
        (glyph, action)
    }

    fn handle_popup_key(&mut self, key: KeyEvent) {
        let popup = self.popup.as_mut().unwrap();
        let len = popup.items.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => self.popup = None,
            KeyCode::Up | KeyCode::Char('k') => step(&mut popup.state, len, -1),
            KeyCode::Down | KeyCode::Char('j') => step(&mut popup.state, len, 1),
            KeyCode::PageUp => step(&mut popup.state, len, -15),
            KeyCode::PageDown => step(&mut popup.state, len, 15),
            KeyCode::Char('s') => self.jump_to_result(),
            KeyCode::Char('o') | KeyCode::Enter | KeyCode::Char('y') => {
                let Some(i) = popup.state.selected() else { return };
                match popup.items[i].fs_path.clone() {
                    Some(path) if key.code == KeyCode::Char('y') => self.copy(&path),
                    Some(path) => reveal(&path),
                    None => self.status = Some("cannot resolve this non-UTF-8 name".into()),
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Entries => {
                let len = self.current().children().len();
                step(self.states.last_mut().unwrap(), len, delta);
            }
            Focus::Breakdown => step(&mut self.breakdown_state, self.breakdown.len(), delta),
        }
    }

    fn enter(&mut self) {
        let Some(i) = self.selected_entry() else { return };
        let child = &self.current().children()[i];
        if !child.is_dir() {
            return;
        }
        let state = first_selected(child.children().len());
        self.stack.push(i);
        self.states.push(state);
        self.refresh_breakdown();
    }

    fn leave(&mut self) {
        if self.stack.pop().is_some() {
            self.states.pop();
            self.refresh_breakdown();
        }
    }

    /// Enter a result directory, or select a result file in its parent. Each
    /// ancestor keeps its selected child, just like ordinary folder entry.
    fn jump_to_result(&mut self) {
        let Some(target) = self.popup.as_ref().and_then(|p| {
            p.state
                .selected()
                .and_then(|i| p.items.get(i))
                .map(|item| item.target.clone())
        }) else {
            return;
        };
        // Validate the whole chain before changing any navigation state.
        // Refresh/re-sort always closes the popup in restore(), so indices
        // cannot outlive the snapshot/order that supplied them.
        let mut node = self.current();
        for &i in &target {
            let Some(child) = node.children().get(i) else {
                self.popup = None;
                self.status = Some("result is no longer in this snapshot".into());
                return;
            };
            node = child;
        }
        self.popup = None;
        self.focus = Focus::Entries;
        for i in target {
            self.states.last_mut().unwrap().select(Some(i));
            let child = &self.current().children()[i];
            if child.is_dir() {
                let state = first_selected(child.children().len());
                self.stack.push(i);
                self.states.push(state);
            }
        }
        self.refresh_breakdown();
    }

    fn open_popup(&mut self) {
        let Some(item) = self.breakdown_state.selected().and_then(|i| self.breakdown.get(i)) else {
            return;
        };
        let base = self.fs_path();
        let cur = self.current();
        let by_dedup = self.size_mode == SizeMode::Dedup;
        let mut top = TopN::new(POPUP_LIMIT);
        let mut title = match &item.key {
            Key::Cat(c) if c.is_dir_kind() => format!("{} directories", c.label()),
            Key::Cat(c) => format!("{} files", c.label()),
            Key::Ext(e) => format!("*.{e} files"),
        };
        walk_popup(
            cur,
            &mut PathBuf::new(),
            &mut Vec::new(),
            &mut |n, dedup, path, indices| {
                let matches = match &item.key {
                    Key::Cat(c) if c.is_dir_kind() => n.special && n.cat == *c,
                    Key::Cat(c) => !n.is_dir() && n.cat == *c,
                    Key::Ext(e) => !n.is_dir() && matches_ext_key(&n.name, e),
                };
                if matches {
                    let size = if by_dedup { dedup } else { n.size };
                    if top.accepts(|min: &PopupItem| min.cmp_rank(size, path, indices).is_lt()) {
                        top.push(PopupItem {
                            size,
                            path: path.to_path_buf(),
                            fs_path: None,
                            target: indices.to_vec(),
                        });
                    }
                }
                // A matching directory represents its whole special subtree.
                !(matches && n.is_dir())
            },
        );
        if by_dedup {
            title.push_str(" · clones counted once");
        }
        let mut items = top.into_sorted();
        // Resolve only final survivors, before exposing the popup. These paths
        // serve existing external actions; jumping still uses snapshot indices.
        for item in &mut items {
            item.fs_path = base.as_deref().and_then(|b| popup_fs_path(cur, b, &item.target));
        }
        let state = first_selected(items.len());
        self.popup = Some(Popup { title, items, state });
    }
}

/// Resolve a retained target using the same raw-name handling as resolve_chain.
/// Missing snapshot indices or unresolvable names leave external actions disabled.
fn popup_fs_path(mut node: &Node, base: &Path, target: &[usize]) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for &index in target {
        node = node.children().get(index)?;
        path = crate::scan::resolve_child(&path, node)?;
    }
    Some(path)
}

/// Carry display paths and snapshot indices with O(depth) traversal state.
/// Only admitted top-N candidates copy them; no filesystem access occurs here.
fn walk_popup(
    node: &Node,
    rel: &mut PathBuf,
    indices: &mut Vec<usize>,
    f: &mut impl FnMut(&Node, u64, &Path, &[usize]) -> bool,
) {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        let Some(dir) = node.dir.as_deref() else { return };
        for (i, child) in dir.children.iter().enumerate() {
            rel.push(&*child.name);
            indices.push(i);
            if f(child, dir.child_dedup(i), rel, indices) && child.is_dir() {
                walk_popup(child, rel, indices, f);
            }
            indices.pop();
            rel.pop();
        }
    });
}

/// Re-sort the dirs whose order can depend on the size mode: those with
/// clones below (elsewhere the deduplicated size equals the size).
fn sort_tree(node: &mut Node, by_dedup: bool) {
    if !node.clone {
        return;
    }
    let Some(d) = node.dir.as_mut() else { return };
    d.sort_children(by_dedup);
    // Keep one remaining-siblings iterator per ancestor, not one pending
    // entry per child. Deep clone trees use O(depth) heap state instead of
    // recursive stack frames, while wide directories need only one frame.
    let mut stack = vec![d.children.iter_mut()];
    while let Some(children) = stack.last_mut() {
        let Some(child) = children.next() else {
            stack.pop();
            continue;
        };
        if !child.clone {
            continue;
        }
        if let Some(d) = child.dir.as_mut() {
            d.sort_children(by_dedup);
            stack.push(d.children.iter_mut());
        }
    }
}

fn step(state: &mut TableState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    state.select(Some(cur.saturating_add(delta).clamp(0, len as isize - 1) as usize));
}

pub fn ext_key(name: &str) -> String {
    category::ext_of(name).map_or_else(|| "(none)".to_string(), |e| e.to_ascii_lowercase())
}

/// Compare to the ASCII-lowercased breakdown key without allocating per file.
/// Fold only the name, not the key: noncanonical keys must stay unmatched.
fn matches_ext_key(name: &str, key: &str) -> bool {
    category::ext_of(name)
        .unwrap_or("(none)")
        .bytes()
        .map(|b| b.to_ascii_lowercase())
        .eq(key.bytes())
}

fn ext_breakdown(cur: &Node) -> Vec<BreakdownItem> {
    let mut map: HashMap<String, (u64, u64, Category)> = HashMap::new();
    walk(cur, &mut PathBuf::new(), &mut |n, _| {
        if !n.is_dir() {
            let e = map
                .entry(ext_key(&n.name))
                .or_insert((0, 0, reg().classify_file(&n.name)));
            e.0 += n.size;
            e.1 += 1;
        }
        true
    });
    let mut items: Vec<_> = map
        .into_iter()
        .map(|(ext, (size, count, cat))| BreakdownItem {
            label: if ext == "(none)" {
                ext.clone()
            } else {
                format!(".{ext}")
            },
            key: Key::Ext(ext),
            cat,
            size,
            count: Some(count),
        })
        .collect();
    // HashMap iteration is arbitrary; equal-size rows and the top-60 cutoff
    // must use the same stable display-label order on every refresh.
    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.label.cmp(&b.label)));
    items.truncate(EXT_LIMIT);
    items
}

/// Show the file in the OS file manager.
fn reveal(path: &Path) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg("-R").arg(path);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{}", path.display()));
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Copy `text` with the OS clipboard tool, or with an OSC 52 escape sequence
/// over SSH or when no tool is found (most terminals, tmux with set-clipboard).
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let var = |k| std::env::var_os(k).is_some();
    let tools: &[&[&str]] = if var("SSH_CONNECTION") || var("SSH_TTY") {
        &[]
    } else if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else if var("WAYLAND_DISPLAY") {
        &[&["wl-copy"]]
    } else {
        &[
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    for tool in tools {
        let child = Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if child.wait().is_ok_and(|s| s.success()) && wrote {
            return Ok(());
        }
    }
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Progress, scan_excluding, scan_subtree};
    use std::fs;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dutype-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(d).unwrap()
    }

    fn full(root: &Path) -> Node {
        scan_excluding(root, true, true, &Progress::default(), None).unwrap()
    }

    fn same(a: &Node, b: &Node) {
        assert_eq!((&a.name, a.size, a.cat, a.special), (&b.name, b.size, b.cat, b.special));
        assert_eq!((a.files(), a.cats()), (b.files(), b.cats()), "{}", a.name);
        assert_eq!(
            (a.dedup(), a.freeable(), a.clone),
            (b.dedup(), b.freeable(), b.clone),
            "{}",
            a.name
        );
        let names = |n: &Node| n.children().iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(a), names(b), "order under {}", a.name);
        a.children().iter().zip(b.children()).for_each(|(x, y)| same(x, y));
    }

    fn jump_file(name: &str, size: u64) -> Node {
        Node {
            name: name.into(),
            size,
            cat: reg().classify_file(name),
            special: false,
            hardlink: false,
            lossy: false,
            clone: false,
            dir: None,
        }
    }

    fn jump_dir(name: &str, children: Vec<Node>) -> Node {
        let mut cats = vec![0; reg().len()].into_boxed_slice();
        for child in &children {
            for (total, size) in cats.iter_mut().zip(child.cats()) {
                *total += size;
            }
        }
        Node {
            name: name.into(),
            size: children.iter().map(|c| c.size).sum(),
            cat: reg().other(),
            special: false,
            hardlink: false,
            lossy: false,
            clone: children.iter().any(|c| c.clone),
            dir: Some(Box::new(crate::scan::DirInfo {
                cats,
                files: children.iter().map(Node::files).sum(),
                errors: 0,
                children,
                free_base: 0,
                clones: None,
            })),
        }
    }

    fn jump_app(root: Node) -> App {
        App::new(root, PathBuf::from("/synthetic-not-on-disk"), Duration::ZERO, 0)
    }

    fn jump_popup(app: &mut App, key: Key) {
        app.focus = Focus::Breakdown;
        app.breakdown = vec![BreakdownItem {
            key,
            label: "fixture".into(),
            cat: reg().other(),
            size: 0,
            count: None,
        }];
        app.breakdown_state.select(Some(0));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
    }

    fn rank_popup(app: &mut App, key: Key) {
        app.popup = None;
        app.focus = Focus::Breakdown;
        app.breakdown = vec![BreakdownItem {
            key,
            label: "fixture".into(),
            cat: reg().other(),
            size: 0,
            count: None,
        }];
        app.breakdown_state.select(Some(0));
        // Exercise collection directly: repeated test opens must never dispatch
        // Enter to an existing popup's external reveal action.
        app.open_popup();
    }

    // Original implementation, kept only as a bounded differential oracle.
    // It runs on the ordinary test thread, never the 128 KiB regression worker.
    fn recursive_sort_tree(node: &mut Node, by_dedup: bool) {
        let Some(d) = node.dir.as_mut() else { return };
        if !node.clone {
            return;
        }
        d.sort_children(by_dedup);
        for child in &mut d.children {
            recursive_sort_tree(child, by_dedup);
        }
    }

    fn assert_same_sort_snapshot(actual: &Node, expected: &Node) {
        let mut pending = vec![(actual, expected)];
        while let Some((a, b)) = pending.pop() {
            assert_eq!(
                (&a.name, a.size, a.cat, a.special, a.hardlink, a.lossy, a.clone),
                (&b.name, b.size, b.cat, b.special, b.hardlink, b.lossy, b.clone)
            );
            assert_eq!((a.dedup(), a.freeable()), (b.dedup(), b.freeable()));
            match (&a.dir, &b.dir) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    assert_eq!(
                        (&a.cats, a.files, a.errors, a.free_base),
                        (&b.cats, b.files, b.errors, b.free_base)
                    );
                    match (&a.clones, &b.clones) {
                        (None, None) => {}
                        (Some(a), Some(b)) => {
                            assert_eq!((a.group_free, a.dup, a.gaps), (b.group_free, b.dup, b.gaps));
                            // Includes all shared-index, inode, clone and reflink fields.
                            assert_eq!(a.shared, b.shared);
                        }
                        _ => panic!("clone bookkeeping presence changed"),
                    }
                    assert_eq!(a.children.len(), b.children.len());
                    for i in 0..a.children.len() {
                        assert_eq!(a.child_dedup(i), b.child_dedup(i));
                        assert_eq!(a.child_freeable(i), b.child_freeable(i));
                        pending.push((&a.children[i], &b.children[i]));
                    }
                }
                _ => panic!("directory presence changed"),
            }
        }
    }

    fn sort_snapshot_fixture() -> Node {
        use crate::scan::{CloneInfo, Reflink, Shared};

        let mut duplicate = jump_file("duplicate.pdf", 8192);
        duplicate.clone = true;
        let mut partial = jump_file("partial.pdf", 8192);
        partial.clone = true;
        let mut representative = jump_file("representative.pdf", 8192);
        representative.clone = true;
        representative.hardlink = true;
        let mut empty = jump_dir("empty", vec![]);
        empty.clone = true;
        let mut same_name = jump_file("tie.pdf", 4096);
        same_name.lossy = true;
        let mut node = jump_dir(
            "mixed",
            vec![
                duplicate,
                partial,
                representative,
                jump_file("z-zero", 0),
                jump_file("a-zero", 0),
                jump_file("tie.pdf", 4096),
                same_name,
                // Intentionally unsorted: a non-clone subtree must be pruned.
                jump_dir("plain", vec![jump_file("z.txt", 1), jump_file("a.txt", 2)]),
                empty,
            ],
        );
        node.special = true;
        node.hardlink = true;
        let dir = node.dir.as_mut().unwrap();
        dir.errors = 3;
        dir.free_base = 17;
        dir.clones = Some(Box::new(CloneInfo {
            group_free: 29,
            dup: 12288,
            gaps: 5,
            shared: (0..3)
                .map(|idx| Shared {
                    idx,
                    key: (7, 11 + u64::from(idx)),
                    is_clone: true,
                    ino: 23 + u64::from(idx),
                    nlink: if idx == 2 { 2 } else { 1 },
                    refcnt: 2,
                    dup: idx == 0,
                    reflink: (idx == 1).then(|| {
                        Box::new(Reflink {
                            extents: vec![(1024, 4096), (8192, 1024)].into_boxed_slice(),
                            dup: 4096,
                            opaque: false,
                        })
                    }),
                })
                .collect(),
        }));
        node
    }

    fn check_sort_against_recursive(mut actual: Node, mut expected: Node) {
        for by_dedup in [false, true, true, false, false, true] {
            sort_tree(&mut actual, by_dedup);
            recursive_sort_tree(&mut expected, by_dedup);
            assert_same_sort_snapshot(&actual, &expected);
        }
    }

    #[test]
    fn size_mode_sort_matches_recursive_snapshot_state() {
        check_sort_against_recursive(sort_snapshot_fixture(), sort_snapshot_fixture());
        let siblings = || jump_dir("root", (0..16).map(|_| sort_snapshot_fixture()).collect());
        check_sort_against_recursive(siblings(), siblings());
        let non_clone = || jump_dir("plain", vec![jump_file("z.txt", 1), jump_file("a.txt", 2)]);
        check_sort_against_recursive(non_clone(), non_clone());
        check_sort_against_recursive(jump_file("file.pdf", 0), jump_file("file.pdf", 0));
        let clone_file = || {
            let mut file = jump_file("clone.pdf", 0);
            file.clone = true;
            file
        };
        check_sort_against_recursive(clone_file(), clone_file());
    }

    #[test]
    fn size_mode_sort_matches_recursive_deep_and_wide_snapshots() {
        let deep = || {
            let mut node = sort_snapshot_fixture();
            // The mixed fixture already has two directory levels.
            for _ in 2..1000 {
                node = jump_dir("d", vec![node]);
                node.dir.as_mut().unwrap().clones_mut().dup = 12288;
            }
            node
        };
        check_sort_against_recursive(deep(), deep());
        let wide = || jump_dir("wide", (0..1000).map(|_| sort_snapshot_fixture()).collect());
        check_sort_against_recursive(wide(), wide());
    }

    // Temporary phase diagnostics are fixed to the three bounded cases below.
    // Use --nocapture so a process-aborting stack overflow preserves the last
    // entered/completed phase. No fixture path is ever created on disk.
    fn deep_tui_phase(depth: usize, stack_bytes: Option<usize>, phase: &str) {
        eprintln!("[deep-tui-phase] depth={depth} requested_stack_bytes={stack_bytes:?} phase={phase}");
    }

    fn check_deep_tui_snapshot(depth: usize, stack_bytes: Option<usize>) {
        use crate::scan::{CloneInfo, Shared};

        deep_tui_phase(depth, stack_bytes, "fixture-construction:begin");
        let mut shared = jump_file("deep.pdf", 8192);
        shared.clone = true;
        let special_cat = reg().find_by_name("node_modules").unwrap();
        let special = |children| {
            let mut node = jump_dir("node_modules", children);
            node.special = true;
            node.cat = special_cat;
            node
        };
        let mut branch = jump_dir(
            "d",
            vec![shared, jump_file("plain.pdf", 4096), special(vec![special(vec![])])],
        );
        branch.dir.as_mut().unwrap().clones = Some(Box::new(CloneInfo {
            dup: 8192,
            shared: vec![Shared {
                idx: 0,
                key: (1, 1),
                is_clone: true,
                ino: 1,
                nlink: 1,
                refcnt: 2,
                dup: true,
                reflink: None,
            }]
            .into_boxed_slice(),
            ..CloneInfo::default()
        }));
        for _ in 1..depth {
            branch = jump_dir("d", vec![branch]);
            branch.dir.as_mut().unwrap().clones_mut().dup = 8192;
        }
        // The shared file's representative is elsewhere in the snapshot.
        // This shape changes both root order and deepest-child order in dedup
        // mode, rather than merely visiting a single-child chain.
        let mut representative = jump_file("sibling.pdf", 8192);
        representative.clone = true;
        let mut root = jump_dir("root", vec![branch, representative]);
        root.dir.as_mut().unwrap().clones = Some(Box::new(CloneInfo {
            dup: 8192,
            shared: vec![Shared {
                idx: 1,
                key: (1, 1),
                is_clone: true,
                ino: 2,
                nlink: 1,
                refcnt: 2,
                dup: false,
                reflink: None,
            }]
            .into_boxed_slice(),
            ..CloneInfo::default()
        }));
        deep_tui_phase(depth, stack_bytes, "fixture-construction:complete");
        let builder = std::thread::Builder::new();
        let builder = match stack_bytes {
            Some(bytes) => builder.stack_size(bytes),
            None => builder,
        };
        deep_tui_phase(depth, stack_bytes, "worker-spawn:begin");
        builder
            .spawn(move || {
                deep_tui_phase(depth, stack_bytes, "app-construction:begin");
                let mut app = jump_app(root);
                deep_tui_phase(depth, stack_bytes, "app-construction:complete");
                app.mode = Mode::Extension;
                deep_tui_phase(depth, stack_bytes, "extension-walk:begin");
                app.refresh_breakdown();
                deep_tui_phase(depth, stack_bytes, "extension-walk:complete");
                assert_eq!(app.breakdown.len(), 1);
                assert_eq!(app.breakdown[0].count, Some(3));
                assert_eq!(app.breakdown[0].size, 20480);
                // Location restoration need not repeatedly aggregate the
                // extension of every ancestor while testing sort identity.
                app.mode = Mode::Category;
                deep_tui_phase(depth, stack_bytes, "initial-dedup-sort-and-restore:begin");
                app.set_size_mode(SizeMode::Dedup);
                deep_tui_phase(depth, stack_bytes, "initial-dedup-sort-and-restore:complete");
                assert_eq!(app.root.children()[0].name.as_ref(), "sibling.pdf");
                assert_eq!((app.root.size, app.root.dedup(), app.root.files()), (20480, 12288, 3));

                assert_eq!(app.root.dir.as_ref().unwrap().shared()[0].idx, 0);
                deep_tui_phase(depth, stack_bytes, "special-popup-walk:begin");
                rank_popup(&mut app, Key::Cat(special_cat));
                deep_tui_phase(depth, stack_bytes, "special-popup-walk:complete");
                let popup = app.popup.as_ref().unwrap();
                assert_eq!(popup.items.len(), 1, "the nested matching directory is pruned");
                assert_eq!(popup.items[0].target.len(), depth + 1);
                assert_eq!(popup.items[0].target[depth], 2);

                deep_tui_phase(depth, stack_bytes, "file-popup-walks:begin");
                for key in [Key::Cat(reg().classify_file("deep.pdf")), Key::Ext("pdf".into())] {
                    rank_popup(&mut app, key);
                    let popup = app.popup.as_mut().unwrap();
                    assert_eq!(
                        popup.items.iter().map(|item| item.size).collect::<Vec<_>>(),
                        [8192, 4096, 0]
                    );
                    let target = &popup.items[2].target;
                    assert_eq!(target.len(), depth + 1);
                    assert_eq!((target[0], target[depth]), (1, 1));
                    assert!(target[1..depth].iter().all(|&i| i == 0));
                    assert!(popup.items[2].path.ends_with("deep.pdf"));
                }
                deep_tui_phase(depth, stack_bytes, "file-popup-walks:complete");
                app.popup.as_mut().unwrap().state.select(Some(2));
                deep_tui_phase(depth, stack_bytes, "snapshot-jump:begin");
                app.jump_to_result();
                deep_tui_phase(depth, stack_bytes, "snapshot-jump:complete");
                assert_eq!(app.stack.len(), depth);
                assert_eq!(app.states.len(), depth + 1);
                assert_eq!(
                    app.current().children()[app.selected_entry().unwrap()].name.as_ref(),
                    "deep.pdf"
                );
                assert_eq!(app.current().dir.as_ref().unwrap().shared()[0].idx, 1);

                // Sorting preserves the location and selected identity while
                // remapping the shared record back to its original file.
                deep_tui_phase(depth, stack_bytes, "du-sort-and-restore:begin");
                app.set_size_mode(SizeMode::Du);
                deep_tui_phase(depth, stack_bytes, "du-sort-and-restore:complete");
                assert_eq!(app.stack[0], 0);
                assert_eq!(app.root.dir.as_ref().unwrap().shared()[0].idx, 1);
                assert_eq!(app.selected_entry(), Some(0));
                assert_eq!(app.current().children()[0].name.as_ref(), "deep.pdf");
                assert_eq!(app.current().dir.as_ref().unwrap().shared()[0].idx, 0);
                deep_tui_phase(depth, stack_bytes, "dedup-sort-and-restore:begin");
                app.set_size_mode(SizeMode::Dedup);
                deep_tui_phase(depth, stack_bytes, "dedup-sort-and-restore:complete");
                assert_eq!(app.stack[0], 1);
                assert_eq!(app.selected_entry(), Some(1));
                deep_tui_phase(depth, stack_bytes, "in-du-order-sorts-and-restore:begin");
                app.in_du_order(|root| assert_eq!(root.children()[0].name.as_ref(), "d"));
                deep_tui_phase(depth, stack_bytes, "in-du-order-sorts-and-restore:complete");
                assert_eq!(app.stack.len(), depth);
                assert_eq!(app.stack[0], 1);
                assert_eq!(app.selected_entry(), Some(1));
                assert_eq!(app.current().dir.as_ref().unwrap().shared()[0].idx, 1);
                assert_eq!((app.root.size, app.root.dedup(), app.root.files()), (20480, 12288, 3));
                // Drop on the same selected stack as all traversal operations.
                deep_tui_phase(depth, stack_bytes, "drop:begin");
                drop(app);
                deep_tui_phase(depth, stack_bytes, "drop:complete");
            })
            .unwrap()
            .join()
            .unwrap();
        deep_tui_phase(depth, stack_bytes, "worker-join:complete");
    }

    #[test]
    fn deep_popup_jump_and_size_modes_preserve_snapshot_on_small_stack() {
        check_deep_tui_snapshot(1000, Some(128 * 1024));
    }

    #[test]
    fn shallow_popup_jump_and_size_modes_preserve_snapshot_on_small_stack() {
        check_deep_tui_snapshot(32, Some(128 * 1024));
    }

    #[test]
    fn deep_popup_jump_and_size_modes_preserve_snapshot_on_default_stack() {
        // Leave Builder::stack_size unset; this is a default Rust worker,
        // not a claim about the process main thread or the production TUI.
        check_deep_tui_snapshot(1000, None);
    }

    #[test]
    fn borrowed_extension_match_is_exactly_normalized_key_equality() {
        let mut names: Vec<String> = [
            "",
            ".",
            "..",
            "README",
            ".bashrc",
            "name.",
            "name..",
            ".config.JSON",
            "archive.tar.GZ",
            "a..PDF",
            "file.PdF",
            "file.(none)",
            "file.(NONE)",
            "file.É",
            "file.é",
            "file.İ",
            "file.ı",
            "file.Σ",
            "file.σ",
            "file.ς",
            "file.ẞ",
            "file.ß",
            "file.K",
            "file.k",
            "file.ÉPDF",
            "file.éPDF",
            "file.e\u{301}PDF",
            "file.日本語",
            "file.🦀",
            r"raw\xff.PDF",
            r"raw.P\xffDF",
            r"raw.PD\x46",
            r"literal\\.PDF",
            "lossy.�PDF",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        // Exercise every ASCII byte, including controls, both before and after
        // the final dot. These are strings only, never filesystem fixtures.
        for byte in 0..=127 {
            let c = char::from(byte);
            names.extend([format!("stem.{c}"), format!("stem.X{c}Z"), format!("{c}.PDF")]);
        }
        let mut keys = vec![
            "".into(),
            "pdf".into(),
            "PDF".into(),
            "PdF".into(),
            "(none)".into(),
            "(NONE)".into(),
            "absent".into(),
        ];
        for name in &names {
            let key = ext_key(name);
            keys.extend([
                key.clone(),
                key.to_ascii_uppercase(),
                key.to_uppercase(),
                key.to_lowercase(),
            ]);
        }
        keys.sort_unstable();
        keys.dedup();
        for name in &names {
            for key in &keys {
                assert_eq!(
                    matches_ext_key(name, key),
                    ext_key(name) == *key,
                    "name={name:?}, key={key:?}"
                );
            }
        }
        assert!(matches_ext_key("file.(NONE)", "(none)"));
        assert!(matches_ext_key("README", "(none)"));
        assert!(!matches_ext_key("file.PDF", "PDF"));
        assert!(!matches_ext_key("README", "(NONE)"));
        assert!(!matches_ext_key("file.É", "é"));
        assert!(!matches_ext_key("file.K", "k"));
    }

    #[test]
    fn borrowed_extension_popup_preserves_oracle_output_and_jump_targets() {
        let mut raw = jump_file(r"raw\xff.PdF", 4);
        // Unix resolution decodes this in memory; avoid filesystem lookup on
        // platforms whose lossy-name representation needs directory reads.
        raw.lossy = cfg!(unix);
        let tree = jump_dir(
            "root",
            vec![
                jump_file("README", 1),
                jump_file(".hidden", 2),
                jump_file("trailing.", 3),
                jump_file("file.(NONE)", 4),
                jump_file("a.PDF", 5),
                jump_file("a.pDf", 5),
                jump_file("archive.tar.GZ", 6),
                jump_file("file.ÉPDF", 7),
                jump_file("file.éPDF", 8),
                jump_file("file.İ", 9),
                jump_file("file.ı", 10),
                jump_file("file.K", 11),
                jump_file("file.k", 12),
                jump_file("file.日本語", 13),
                jump_file("file.�PDF", 14),
                raw,
                jump_file(r"raw\xff.PdF", 4),
                jump_file(r"raw.P\xffDF", 4),
                jump_dir(
                    "directory.PDF",
                    vec![jump_file("child.pdf", 15), jump_file("child", 16)],
                ),
                jump_dir("empty.(none)", vec![]),
            ],
        );
        let mut app = jump_app(tree);
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            for key in [
                "pdf",
                "PDF",
                "PdF",
                "(none)",
                "(NONE)",
                "gz",
                "GZ",
                "Épdf",
                "épdf",
                "İ",
                "i",
                "ı",
                "K",
                "k",
                "日本語",
                "�pdf",
                r"p\xffdf",
                "",
                "absent",
            ] {
                let mut expected = Vec::new();
                walk_popup(
                    &app.root,
                    &mut PathBuf::new(),
                    &mut Vec::new(),
                    &mut |n, dedup, path, indices| {
                        // The allocating expression is the pre-optimization oracle.
                        if !n.is_dir() && ext_key(&n.name) == key {
                            expected.push(PopupItem {
                                path: path.to_path_buf(),
                                size: if mode == SizeMode::Dedup { dedup } else { n.size },
                                fs_path: popup_fs_path(&app.root, &app.root_path, indices),
                                target: indices.to_vec(),
                            });
                        }
                        true
                    },
                );
                expected.sort_by(|a, b| b.cmp(a));
                rank_popup(&mut app, Key::Ext(key.into()));
                let popup = app.popup.as_ref().unwrap();
                let suffix = if mode == SizeMode::Dedup {
                    " · clones counted once"
                } else {
                    ""
                };
                assert_eq!(popup.title, format!("*.{key} files{suffix}"));
                assert_eq!(popup.items.len(), expected.len(), "key={key:?}");
                for (got, want) in popup.items.iter().zip(&expected) {
                    assert_eq!(
                        (&got.path, got.size, &got.fs_path, &got.target),
                        (&want.path, want.size, &want.fs_path, &want.target),
                        "key={key:?}"
                    );
                    assert!(!node_at(&app.root, &got.target).is_dir());
                }
                for (index, want) in expected.iter().enumerate() {
                    rank_popup(&mut app, Key::Ext(key.into()));
                    app.popup.as_mut().unwrap().state.select(Some(index));
                    // Exercise only the in-app jump, never reveal/open/copy.
                    app.handle_popup_key(KeyEvent::from(KeyCode::Char('s')));
                    assert!(app.popup.is_none());
                    let mut target = app.stack.clone();
                    target.push(app.selected_entry().unwrap());
                    assert_eq!(target, want.target);
                    consistent_history(&app);
                    while !app.stack.is_empty() {
                        app.leave();
                    }
                }
            }
        }
    }

    #[test]
    fn popup_top_500_ties_match_report_paths() {
        let mut app = jump_app(jump_dir(
            "root",
            (0..501).map(|i| jump_file(&format!("f{i:04}.pdf"), 1)).collect(),
        ));
        let cat = reg().classify_file("a.pdf");
        let expected = crate::report::find(&app.root, Path::new(""), cat, POPUP_LIMIT, SizeMode::Du, false, None);
        for key in [Key::Cat(cat), Key::Ext("pdf".into())] {
            rank_popup(&mut app, key);
            let got: Vec<_> = app
                .popup
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|i| i.path.clone())
                .collect();
            assert_eq!(got.first().unwrap(), Path::new("f0500.pdf"));
            assert_eq!(got.last().unwrap(), Path::new("f0001.pdf"));
            assert_eq!(
                got,
                expected
                    .items
                    .iter()
                    .map(|i| PathBuf::from(&i.path))
                    .collect::<Vec<_>>()
            );
        }
    }

    fn assert_popup_report(app: &App, cat: Category) {
        let expected = crate::report::find(
            app.current(),
            Path::new(""),
            cat,
            POPUP_LIMIT,
            app.size_mode,
            false,
            None,
        );
        let actual: Vec<_> = app
            .popup
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| (i.path.clone(), i.size))
            .collect();
        let expected: Vec<_> = expected
            .items
            .iter()
            .map(|i| {
                (
                    PathBuf::from(&i.path),
                    if app.size_mode == SizeMode::Dedup {
                        i.dedup_bytes
                    } else {
                        i.size_bytes
                    },
                )
            })
            .collect();
        assert_eq!(actual, expected);
        for item in &app.popup.as_ref().unwrap().items {
            assert_eq!(item.fs_path, Some(app.root_path.join(&item.path)));
        }
    }

    #[test]
    fn popup_tie_boundaries_zero_and_size_modes() {
        let cat = reg().classify_file("a.pdf");
        for count in [499, 500, 501] {
            for size in [0, 1] {
                for mode in [SizeMode::Du, SizeMode::Dedup] {
                    let mut app = jump_app(jump_dir(
                        "root",
                        (0..count).map(|i| jump_file(&format!("f{i:04}.pdf"), size)).collect(),
                    ));
                    app.set_size_mode(mode);
                    for key in [Key::Cat(cat), Key::Ext("pdf".into())] {
                        rank_popup(&mut app, key);
                        assert_popup_report(&app, cat);
                        let popup = app.popup.as_ref().unwrap();
                        let expected: Vec<_> = (0..count)
                            .rev()
                            .take(POPUP_LIMIT)
                            .map(|i| PathBuf::from(format!("f{i:04}.pdf")))
                            .collect();
                        assert_eq!(popup.items.iter().map(|i| i.path.clone()).collect::<Vec<_>>(), expected);
                        for item in &popup.items {
                            assert_eq!(Path::new(&*node_at(app.current(), &item.target).name), item.path);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn popup_mixed_sizes_and_nested_path_order_match_report() {
        let cat = reg().classify_file("a.pdf");
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            let mut nodes: Vec<_> = (0..503).map(|i| jump_file(&format!("f{i:04}.pdf"), i % 5)).collect();
            nodes.push(jump_file("ignored.zip", 1000));
            nodes.push(jump_dir("a", vec![jump_file("z.pdf", 10)]));
            nodes.push(jump_file("a-.pdf", 10));
            let mut app = jump_app(jump_dir("root", nodes));
            app.set_size_mode(mode);
            for key in [Key::Cat(cat), Key::Ext("pdf".into())] {
                rank_popup(&mut app, key);
                assert_popup_report(&app, cat);
                let popup = app.popup.as_ref().unwrap();
                // Path components, not flat slash strings: "a-" sorts after "a".
                assert_eq!(popup.items[0].path, Path::new("a-.pdf"));
                assert_eq!(popup.items[1].path, Path::new("a").join("z.pdf"));
                assert!(popup.items.iter().all(|i| i.path != Path::new("ignored.zip")));
            }
        }
    }

    #[test]
    fn popup_directory_cutoff_prunes_nested_matches_and_keeps_targets() {
        let cat = reg().find_by_name("node_modules").unwrap();
        let special = |children| {
            let mut n = jump_dir("node_modules", children);
            n.cat = cat;
            n.special = true;
            n.dir.as_mut().unwrap().cats[cat.index()] = n.size;
            n
        };
        let root = jump_dir(
            "root",
            (0..501)
                .map(|i| {
                    jump_dir(
                        &format!("p{i:04}"),
                        vec![special(vec![special(vec![jump_file("a.js", 1)])])],
                    )
                })
                .collect(),
        );
        let mut app = jump_app(root);
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            rank_popup(&mut app, Key::Cat(cat));
            assert_popup_report(&app, cat);
            let popup = app.popup.as_mut().unwrap();
            assert_eq!(popup.items.len(), POPUP_LIMIT);
            assert_eq!(popup.items[0].target, [500, 0]);
            assert_eq!(popup.items[499].target, [1, 0]);
            assert!(popup.items.iter().all(|i| i.target.len() == 2));
            popup.state.select(Some(499));
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.stack, [1, 0]);
            consistent_history(&app);
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Backspace);
        }
    }

    #[test]
    fn popup_dedup_cutoff_uses_effective_size_after_resort() {
        let cat = reg().classify_file("a.pdf");
        let mut root = jump_dir(
            "root",
            (0..501).map(|i| jump_file(&format!("f{i:04}.pdf"), 10)).collect(),
        );
        root.clone = true;
        root.dir.as_mut().unwrap().children[500].clone = true;
        let clones = root.dir.as_mut().unwrap().clones_mut();
        clones.dup = 10;
        clones.shared = vec![crate::scan::Shared {
            idx: 500,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 1,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        let mut app = jump_app(root);
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            for key in [Key::Cat(cat), Key::Ext("pdf".into())] {
                rank_popup(&mut app, key);
                assert_popup_report(&app, cat);
                let popup = app.popup.as_mut().unwrap();
                let expected = if mode == SizeMode::Du { "f0500.pdf" } else { "f0499.pdf" };
                assert_eq!(popup.items[0].path, Path::new(expected));
                assert_eq!(
                    popup.items.iter().any(|i| i.path == Path::new("f0500.pdf")),
                    mode == SizeMode::Du
                );
                popup.state.select(Some(0));
                press(&mut app, KeyCode::Char('s'));
                assert_eq!(&*app.current().children()[app.selected_entry().unwrap()].name, expected);
                consistent_history(&app);
            }
        }
    }

    #[test]
    fn popup_exact_display_collisions_keep_earliest_snapshot_identities() {
        let cat = reg().classify_file("same.pdf");
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            let mut app = jump_app(jump_dir("root", (0..501).map(|_| jump_file("same.pdf", 0)).collect()));
            app.set_size_mode(mode);
            for key in [Key::Cat(cat), Key::Ext("pdf".into())] {
                rank_popup(&mut app, key);
                let popup = app.popup.as_mut().unwrap();
                assert_eq!(popup.items.len(), POPUP_LIMIT);
                assert_eq!(
                    popup.items.iter().map(|i| i.target.clone()).collect::<Vec<_>>(),
                    (0..500).map(|i| vec![i]).collect::<Vec<_>>()
                );
                popup.state.select(Some(499));
                press(&mut app, KeyCode::Char('s'));
                assert_eq!(app.selected_entry(), Some(499));
                consistent_history(&app);
            }
        }
    }

    #[test]
    fn popup_rank_ignores_filesystem_path_payload() {
        let item = |fs, target| PopupItem {
            size: 1,
            path: "same.pdf".into(),
            fs_path: fs,
            target,
        };
        let a = item(Some("z".into()), vec![0]);
        let b = item(Some("a".into()), vec![0]);
        let c = item(None, vec![0]);
        assert!(a == b && b == c);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert!(a > item(Some("zz".into()), vec![1]));
        assert!(item(None, vec![0, 2]) > item(None, vec![1, 0]));
        assert!(a.cmp_rank(1, Path::new("z.pdf"), &[0]).is_lt());
        assert!(a.cmp_rank(2, Path::new("a.pdf"), &[0]).is_lt());
    }

    #[test]
    fn popup_final_paths_preserve_resolution_and_unavailable_semantics() {
        let base = temp("popup-final-paths");
        let unresolved_name = if cfg!(unix) { r"bad\q" } else { "unresolvable" };
        let mut raw = jump_dir(unresolved_name, vec![jump_file("same.pdf", 1)]);
        raw.lossy = true;
        let root = jump_dir(
            "root",
            vec![jump_dir("missing", vec![jump_file("ordinary.pdf", 2)]), raw],
        );
        let mut app = App::new(root, base.clone(), Duration::ZERO, 0);
        let cur = app.current();
        let dir = &cur.children()[0];
        let file = &dir.children()[0];
        assert_eq!(
            popup_fs_path(cur, &base, &[0, 0]),
            crate::scan::resolve_chain(&base, &[dir], file)
        );
        assert_eq!(
            popup_fs_path(cur, &base, &[0, 0]),
            Some(base.join("missing/ordinary.pdf"))
        );
        assert_eq!(popup_fs_path(cur, &base, &[1, 0]), None);
        assert_eq!(popup_fs_path(cur, &base, &[999]), None);
        assert_eq!(popup_fs_path(cur, &base, &[0, 0, 0]), None);
        rank_popup(&mut app, Key::Ext("pdf".into()));
        assert_eq!(
            app.popup.as_ref().unwrap().items[0].fs_path,
            Some(base.join("missing/ordinary.pdf"))
        );
        assert_eq!(app.popup.as_ref().unwrap().items[1].fs_path, None);
        // The current base can itself be unresolved; retained snapshot targets still jump.
        app.go_to(&[Step {
            name: unresolved_name.into(),
            lossy: true,
        }]);
        assert!(app.fs_path().is_none());
        rank_popup(&mut app, Key::Ext("pdf".into()));
        assert!(app.popup.as_ref().unwrap().items[0].fs_path.is_none());
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.stack, [1]);
        assert_eq!(app.selected_entry(), Some(0));
        consistent_history(&app);
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn popup_final_paths_preserve_raw_and_literal_display_twins() {
        use std::os::unix::ffi::OsStringExt;
        let base = temp("popup-raw-twins");
        let raw_dir = std::ffi::OsString::from_vec(b"d\xffir".to_vec());
        let raw_file = std::ffi::OsString::from_vec(b"f\xff.pdf".to_vec());
        let literal_dir = r"d\xffir";
        let literal_file = r"f\xff.pdf";
        // Unix resolution only decodes and joins. Keep this synthetic because
        // macOS filesystems need not allow invalid UTF-8 entry names; Linux PTY
        // coverage separately exercises actual raw names on disk.
        let twin = |lossy| {
            let mut file = jump_file(literal_file, 1);
            file.lossy = true;
            let mut dir = jump_dir(literal_dir, vec![file, jump_file(literal_file, 1)]);
            dir.lossy = lossy;
            dir
        };
        let mut app = App::new(
            jump_dir("root", vec![twin(true), twin(false)]),
            base.clone(),
            Duration::ZERO,
            0,
        );
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            for key in [Key::Cat(reg().classify_file("a.pdf")), Key::Ext("pdf".into())] {
                rank_popup(&mut app, key);
                let popup = app.popup.as_ref().unwrap();
                assert_eq!(popup.items.len(), 4);
                for (i, item) in popup.items.iter().enumerate() {
                    assert_eq!(item.target, [i / 2, i % 2]);
                    let dir = &app.root.children()[i / 2];
                    let file = &dir.children()[i % 2];
                    assert_eq!(item.fs_path, crate::scan::resolve_chain(&base, &[dir], file));
                    let expected_dir = if i / 2 == 0 {
                        base.join(&raw_dir)
                    } else {
                        base.join(literal_dir)
                    };
                    let expected = if i % 2 == 0 {
                        expected_dir.join(&raw_file)
                    } else {
                        expected_dir.join(literal_file)
                    };
                    assert_eq!(item.fs_path.as_deref(), Some(expected.as_path()));
                }
            }
        }
        fs::remove_dir_all(base).unwrap();
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::from(code));
    }

    fn consistent_history(app: &App) {
        assert_eq!(app.states.len(), app.stack.len() + 1);
        for (depth, &index) in app.stack.iter().enumerate() {
            assert_eq!(app.states[depth].selected(), Some(index));
        }
        for depth in 0..app.states.len() {
            let node = node_at(&app.root, &app.stack[..depth]);
            match app.states[depth].selected() {
                Some(i) => assert!(i < node.children().len()),
                None => assert!(node.children().is_empty()),
            }
        }
        assert!(app.rescan.is_none());
        assert!(!app.quit);
    }

    #[test]
    fn extension_ties_cover_zero_unicode_counts_and_cutoff() {
        let root = jump_dir(
            "root",
            vec![
                jump_file("a", 0),
                jump_file("a.ZIP", 0),
                jump_file("b.zip", 0),
                jump_file("a.é", 0),
                jump_file("a.中", 0),
                jump_file("large.rs", 1),
            ],
        );
        let rows = ext_breakdown(&root);
        assert_eq!(
            rows.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            [".rs", "(none)", ".zip", ".é", ".中"]
        );
        assert_eq!(rows.iter().find(|i| i.label == ".zip").unwrap().count, Some(2));
        for count in [59, 60, 61] {
            let root = jump_dir(
                "root",
                (0..count).rev().map(|i| jump_file(&format!("f.e{i:02}"), 0)).collect(),
            );
            let expected: Vec<_> = (0..count.min(EXT_LIMIT)).map(|i| format!(".e{i:02}")).collect();
            for _ in 0..16 {
                assert_eq!(
                    ext_breakdown(&root).into_iter().map(|i| i.label).collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }

    #[test]
    fn extension_ties_are_alphabetical_across_refreshes() {
        let mut app = jump_app(jump_dir(
            "root",
            vec![
                jump_file("x.zip", 4096),
                jump_file("x.pdf", 4096),
                jump_file("x.mp4", 4096),
                jump_file("x.rs", 4096),
            ],
        ));
        app.clones = true;
        app.root.clone = true;
        app.root.dir.as_mut().unwrap().children[0].clone = true;
        let clones = app.root.dir.as_mut().unwrap().clones_mut();
        clones.dup = 4096;
        clones.shared = vec![crate::scan::Shared {
            idx: 0,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 1,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        for _ in 0..32 {
            app.mode = Mode::Extension;
            app.refresh_breakdown();
            let labels: Vec<_> = app.breakdown.iter().map(|i| i.label.as_str()).collect();
            assert_eq!(labels, [".mp4", ".pdf", ".rs", ".zip"]);
            press(&mut app, KeyCode::Char('m'));
            press(&mut app, KeyCode::Char('m'));
            let before = app.size_mode;
            press(&mut app, KeyCode::Char('c'));
            assert_ne!(app.size_mode, before);
            assert_eq!(
                app.shown_total(&app.root),
                if app.size_mode == SizeMode::Du { 16384 } else { 12288 }
            );
            assert!(app.breakdown.iter().all(|item| item.size == 4096));
        }
    }

    #[test]
    fn result_jump_files_preserve_nested_history_and_repeat() {
        for mode in [Mode::Category, Mode::Extension] {
            let mut app = jump_app(jump_dir(
                "root",
                vec![jump_dir(
                    "start",
                    vec![
                        jump_file("direct.pdf", 1),
                        jump_dir("inner", vec![jump_file("small.pdf", 2), jump_file("target.pdf", 10)]),
                    ],
                )],
            ));
            app.go_to(&["start".into()]);
            app.mode = mode;
            let key = || match mode {
                Mode::Category => Key::Cat(reg().classify_file("target.pdf")),
                Mode::Extension => Key::Ext("pdf".into()),
            };
            jump_popup(&mut app, key());
            assert_eq!(app.popup.as_ref().unwrap().items[0].target, [1, 1]);
            press(&mut app, KeyCode::Char('s'));
            assert!(app.popup.is_none());
            assert!(app.focus == Focus::Entries);
            assert_eq!(app.stack, [0, 1]);
            assert_eq!(app.selected_entry(), Some(1));
            consistent_history(&app);
            // Jumping to the same file from its own folder must not add history.
            jump_popup(&mut app, key());
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.stack, [0, 1]);
            assert_eq!(app.selected_entry(), Some(1));
            consistent_history(&app);
            press(&mut app, KeyCode::Backspace);
            assert_eq!(app.stack, [0]);
            assert_eq!(app.selected_entry(), Some(1));
            jump_popup(&mut app, key());
            let popup = app.popup.as_mut().unwrap();
            let direct = popup
                .items
                .iter()
                .position(|i| i.path == Path::new("direct.pdf"))
                .unwrap();
            popup.state.select(Some(direct));
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.stack, [0]);
            assert_eq!(app.selected_entry(), Some(0));
            consistent_history(&app);
            press(&mut app, KeyCode::Left);
            assert!(app.stack.is_empty());
            assert_eq!(app.selected_entry(), Some(0));
        }
    }

    #[test]
    fn result_jump_directories_include_empty_and_prune_nested_matches() {
        let cat = reg().find_by_name("node_modules").unwrap();
        let special = |children| {
            let mut n = jump_dir("node_modules", children);
            n.cat = cat;
            n.special = true;
            n.dir.as_mut().unwrap().cats[cat.index()] = n.size;
            n
        };
        let mut app = jump_app(jump_dir(
            "root",
            vec![
                jump_dir("project", vec![special(vec![special(vec![jump_file("index.js", 10)])])]),
                jump_dir("empty-project", vec![special(vec![])]),
            ],
        ));
        for (row, expected) in [(0, vec![0, 0]), (1, vec![1, 0])] {
            jump_popup(&mut app, Key::Cat(cat));
            assert_eq!(app.popup.as_ref().unwrap().items.len(), 2);
            app.popup.as_mut().unwrap().state.select(Some(row));
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.stack, expected);
            assert_eq!(app.selected_entry(), if row == 0 { Some(0) } else { None });
            consistent_history(&app);
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Left);
            assert!(app.stack.is_empty());
        }
    }

    #[test]
    fn result_jump_uses_indices_for_colliding_display_names_without_fs_paths() {
        // Unix renders invalid bytes as backslash escapes; Windows renders
        // unpaired surrogates as U+FFFD. A Unix escape is not a valid Windows
        // entry name: its backslash would add another PathBuf component.
        let (dir_name, file_name) = if cfg!(unix) {
            ("d\\xffir", "f\\xff.pdf")
        } else {
            ("d\u{fffd}ir", "f\u{fffd}.pdf")
        };
        for name in [dir_name, file_name] {
            assert_eq!(
                Path::new(name).components().count(),
                1,
                "fixture must be one entry name"
            );
        }
        let tree = || {
            let twin = || jump_dir(dir_name, vec![jump_file(file_name, 5), jump_file(file_name, 5)]);
            let mut raw = twin();
            raw.lossy = true;
            raw.dir.as_mut().unwrap().children[0].lossy = true;
            let mut literal = twin();
            literal.dir.as_mut().unwrap().children[0].lossy = true;
            jump_dir("root", vec![raw, literal])
        };
        for mode in [Mode::Category, Mode::Extension] {
            for row in 0..4 {
                let mut app = jump_app(tree());
                let key = if mode == Mode::Category {
                    Key::Cat(reg().classify_file("a.pdf"))
                } else {
                    Key::Ext("pdf".into())
                };
                jump_popup(&mut app, key);
                let popup = app.popup.as_mut().unwrap();
                assert_eq!(popup.items.len(), 4);
                assert!(popup.items.iter().all(|i| i.path == popup.items[0].path));
                let expected = popup.items[row].target.clone();
                popup.items[row].fs_path = None;
                popup.state.select(Some(row));
                press(&mut app, KeyCode::Char('s'));
                assert_eq!(app.stack, expected[..1]);
                assert_eq!(app.selected_entry(), Some(expected[1]));
                assert_eq!(app.current().lossy, expected[0] == 0);
                assert_eq!(app.current().children()[expected[1]].lossy, expected[1] == 0);
                consistent_history(&app);
            }
        }
    }

    #[test]
    fn result_jump_empty_unmatched_and_invalid_targets_do_not_navigate() {
        let mut app = jump_app(jump_dir("root", vec![jump_file("a.pdf", 1)]));
        jump_popup(&mut app, Key::Ext("absent".into()));
        press(&mut app, KeyCode::Char('s'));
        assert!(app.popup.is_some());
        consistent_history(&app);
        press(&mut app, KeyCode::Esc);
        for selected in [None, Some(100)] {
            jump_popup(&mut app, Key::Ext("pdf".into()));
            app.popup.as_mut().unwrap().state.select(selected);
            press(&mut app, KeyCode::Char('s'));
            assert!(app.popup.is_some());
            consistent_history(&app);
            press(&mut app, KeyCode::Esc);
        }
        for target in [vec![100], vec![0, 0]] {
            jump_popup(&mut app, Key::Ext("pdf".into()));
            app.popup.as_mut().unwrap().items[0].target = target;
            press(&mut app, KeyCode::Char('s'));
            assert!(app.popup.is_none());
            assert!(app.status.as_deref().unwrap().contains("no longer"));
            consistent_history(&app);
        }
    }

    #[test]
    fn result_jump_zero_sizes_ties_and_limit_keep_targets_aligned() {
        let mut app = jump_app(jump_dir(
            "root",
            (0..POPUP_LIMIT + 5)
                .map(|i| jump_file(&format!("{i}.pdf"), 0))
                .collect(),
        ));
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            jump_popup(&mut app, Key::Ext("pdf".into()));
            let popup = app.popup.as_ref().unwrap();
            assert_eq!(popup.items.len(), POPUP_LIMIT);
            for item in &popup.items {
                assert_eq!(item.size, 0);
                assert_eq!(Path::new(&*node_at(&app.root, &item.target).name), item.path);
            }
            let target = popup.items.last().unwrap().target[0];
            app.popup.as_mut().unwrap().state.select(Some(POPUP_LIMIT - 1));
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.selected_entry(), Some(target));
            consistent_history(&app);
        }
    }

    #[test]
    fn result_jump_dedup_zero_result_targets_resorted_file() {
        let mut large = jump_file("large.pdf", 100);
        large.clone = true;
        let mut root = jump_dir("root", vec![large, jump_file("small.pdf", 10)]);
        let clones = root.dir.as_mut().unwrap().clones_mut();
        clones.dup = 100;
        clones.shared = vec![crate::scan::Shared {
            idx: 0,
            key: (1, 1),
            is_clone: true,
            ino: 1,
            nlink: 1,
            refcnt: 1,
            dup: true,
            reflink: None,
        }]
        .into_boxed_slice();
        let mut app = jump_app(root);
        for (mode, expected_sizes) in [(SizeMode::Du, [100, 10]), (SizeMode::Dedup, [10, 0])] {
            app.set_size_mode(mode);
            jump_popup(&mut app, Key::Ext("pdf".into()));
            let popup = app.popup.as_mut().unwrap();
            assert_eq!(popup.items.iter().map(|i| i.size).collect::<Vec<_>>(), expected_sizes);
            let row = popup
                .items
                .iter()
                .position(|i| i.path == Path::new("large.pdf"))
                .unwrap();
            popup.state.select(Some(row));
            press(&mut app, KeyCode::Char('s'));
            let selected = app.selected_entry().unwrap();
            assert_eq!(app.current().children()[selected].name.as_ref(), "large.pdf");
            assert_eq!(selected, if mode == SizeMode::Du { 0 } else { 1 });
            consistent_history(&app);
        }
    }

    #[test]
    fn result_jump_invalidates_on_refresh_sort_and_mutable_tree_access() {
        let tree = || {
            let mut large = jump_dir("large", vec![jump_file("large.pdf", 100)]);
            large.clone = true;
            large.dir.as_mut().unwrap().clones_mut().dup = 100;
            jump_dir("root", vec![large, jump_dir("small", vec![jump_file("small.pdf", 10)])])
        };
        let mut app = jump_app(tree());
        jump_popup(&mut app, Key::Ext("pdf".into()));
        app.set_size_mode(SizeMode::Dedup);
        assert!(app.popup.is_none());
        assert_eq!(app.root.children()[0].name.as_ref(), "small");
        jump_popup(&mut app, Key::Ext("pdf".into()));
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.current().name.as_ref(), "large");
        consistent_history(&app);
        press(&mut app, KeyCode::Left);
        jump_popup(&mut app, Key::Ext("pdf".into()));
        app.replace_current(jump_dir("replacement", vec![jump_file("new.pdf", 2)]), false);
        assert!(app.popup.is_none());
        press(&mut app, KeyCode::Char('s'));
        assert!(app.stack.is_empty());
        for mode in [SizeMode::Du, SizeMode::Dedup] {
            app.set_size_mode(mode);
            jump_popup(&mut app, Key::Ext("pdf".into()));
            app.in_du_order(|_| ());
            assert!(app.popup.is_none());
        }
    }

    #[test]
    fn result_jump_leaves_reveal_copy_and_cancel_keys_unchanged() {
        let mut app = jump_app(jump_dir("root", vec![jump_file("a.pdf", 1)]));
        for code in [KeyCode::Enter, KeyCode::Char('o'), KeyCode::Char('y')] {
            jump_popup(&mut app, Key::Ext("pdf".into()));
            app.popup.as_mut().unwrap().items[0].fs_path = None;
            press(&mut app, code); // Cannot invoke an external action without a filesystem path.
            assert_eq!(app.status.as_deref(), Some("cannot resolve this non-UTF-8 name"));
            assert!(app.popup.is_some());
            assert!(app.stack.is_empty());
            app.popup = None;
        }
        for code in [KeyCode::Esc, KeyCode::Left, KeyCode::Backspace, KeyCode::Char('q')] {
            jump_popup(&mut app, Key::Ext("pdf".into()));
            press(&mut app, code);
            assert!(app.popup.is_none());
            consistent_history(&app);
        }
        jump_popup(&mut app, Key::Ext("pdf".into()));
        app.show_keys = true;
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.last_key, Some(("s".into(), "jump in TUI")));
    }

    #[test]
    fn result_jump_survives_tiny_render_and_resize() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = jump_app(jump_dir("root", vec![jump_dir("inner", vec![jump_file("a.pdf", 1)])]));
        jump_popup(&mut app, Key::Ext("pdf".into()));
        for (width, height) in [(120, 32), (40, 10), (20, 5), (1, 1), (0, 0), (80, 24), (120, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            if width >= 40 {
                let text: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
                assert!(text.contains("jump in TUI"));
            }
            assert_eq!(app.popup.as_ref().unwrap().state.selected(), Some(0));
        }
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.stack, [0]);
        consistent_history(&app);
        press(&mut app, KeyCode::Backspace);
        assert!(app.stack.is_empty());
    }

    /// `r` keeps private sizes of the rest of the tree, replaces the
    /// rescanned folder's, and never turns incomplete coverage complete.
    #[test]
    fn rescan_merges_private_sizes() {
        use crate::scan::PrivateSizes;
        let root = temp("private");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        fs::write(root.join("sub/g"), b"x").unwrap();
        let table = |e: &[(&str, u64)]| PrivateSizes(e.iter().map(|&(p, b)| (p.split('/').collect(), b)).collect());
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        (app.private_sizes, app.privates) = (true, table(&[("f", 1), ("sub/g", 2)]));
        app.go_to(&["sub".into()]);
        // A folder whose lookups the volume could not answer.
        app.merge_private(true, true, table(&[]));
        assert_eq!(app.privates, table(&[("f", 1)]));
        assert!(app.private_missing);
        // A later complete rescan of a folder does not hide that.
        app.merge_private(true, false, table(&[("g", 3)]));
        assert_eq!(app.privates, table(&[("f", 1), ("sub/g", 3)]));
        assert!(app.private_sizes && app.private_missing);
        // A rescan of the root replaces the coverage.
        app.leave();
        app.merge_private(true, false, table(&[("f", 4)]));
        assert_eq!(app.privates, table(&[("f", 4)]));
        assert!(app.private_sizes && !app.private_missing);
    }

    /// `r` splices a rescan of the current folder; the result must equal a
    /// full rescan (sizes, counts, categories and sort order of ancestors).
    #[test]
    fn rescan_current_matches_full_scan() {
        let root = temp("splice");
        fs::create_dir_all(root.join("small/inner")).unwrap();
        fs::create_dir_all(root.join("big")).unwrap();
        fs::write(root.join("big/a.mp4"), vec![0u8; 300_000]).unwrap();
        fs::write(root.join("small/inner/b.pdf"), vec![0u8; 10_000]).unwrap();

        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.go_to(&["small".into(), "inner".into()]);
        assert_eq!(app.location(), vec![Step::from("small"), Step::from("inner")]);

        // "small" grows past "big", so the root's order has to change.
        fs::write(root.join("small/inner/c.zip"), vec![0u8; 900_000]).unwrap();
        let fresh = scan_subtree(
            &app.current_path(),
            true,
            true,
            &Progress::default(),
            app.inherited_category(),
            None,
        )
        .unwrap();
        app.replace_current(fresh, false);

        same(&app.root, &full(&root));
        assert_eq!(app.location(), vec![Step::from("small"), Step::from("inner")]);
        assert_eq!(app.root.children()[app.stack[0]].name.as_ref(), "small");
    }

    #[test]
    fn rescan_inside_special_dir_keeps_category() {
        let root = temp("special");
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), vec![0u8; 5_000]).unwrap();
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.go_to(&["node_modules".into(), "pkg".into()]);
        fs::write(root.join("node_modules/pkg/extra.js"), vec![0u8; 5_000]).unwrap();
        let fresh = scan_subtree(
            &app.current_path(),
            true,
            true,
            &Progress::default(),
            app.inherited_category(),
            None,
        )
        .unwrap();
        app.replace_current(fresh, false);
        same(&app.root, &full(&root));
    }

    /// Errors of the rescanned folder replace its old share instead of adding up.
    #[cfg(unix)]
    #[test]
    fn rescan_replaces_error_counts() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("errors");
        let locked = root.join("small/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.errors = app.root.errors();
        assert_eq!(app.errors, 1);
        app.go_to(&["small".into()]);
        let rescan = |app: &App| {
            scan_subtree(
                &app.current_path(),
                true,
                true,
                &Progress::default(),
                app.inherited_category(),
                None,
            )
            .unwrap()
        };
        for _ in 0..3 {
            let fresh = rescan(&app);
            app.replace_current(fresh, false);
            assert_eq!(app.errors, 1);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let fresh = rescan(&app);
        app.replace_current(fresh, false);
        assert_eq!(app.errors, 0);
    }

    /// A subtree with hard links is reported so the caller rescans everything.
    #[cfg(unix)]
    #[test]
    fn subtree_reports_hard_links() {
        let root = temp("hardlinks");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f.bin"), vec![0u8; 50_000]).unwrap();
        let sub = |p: &str| scan_subtree(&root.join(p), true, true, &Progress::default(), None, None).unwrap();
        assert!(!sub("a").hardlink);
        fs::hard_link(root.join("a/f.bin"), root.join("b/f.bin")).unwrap();
        assert!(sub("b").hardlink);
        // The old tree remembers hard links, so deleting the local link
        // before `r` still forces a full rescan.
        let app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        let a = app.root.children().iter().find(|c| &*c.name == "a").unwrap();
        assert!(a.hardlink && app.root.hardlink);
    }

    /// A cancelled scan stops descending right away.
    #[test]
    fn cancelled_scan_stops() {
        let root = temp("cancel");
        fs::create_dir_all(root.join("x/y")).unwrap();
        fs::write(root.join("x/y/z.txt"), "z").unwrap();
        let progress = Progress::default();
        progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let node = scan_excluding(&root, true, true, &progress, None).unwrap();
        assert!(node.children().is_empty());
    }

    /// Directories inside a special dir of category Other (user rule) keep
    /// inheriting Other after `r`, like in a full scan.
    #[test]
    fn rescan_inherits_from_nearest_special_ancestor() {
        let root = temp("inherit");
        fs::create_dir_all(root.join("node_modules/pkg/lib")).unwrap();
        fs::write(root.join("node_modules/pkg/lib/x.mp4"), "x").unwrap();
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.go_to(&["node_modules".into(), "pkg".into(), "lib".into()]);
        let nm = app
            .root
            .children()
            .iter()
            .find(|c| &*c.name == "node_modules")
            .unwrap()
            .cat;
        assert_eq!(app.inherited_category(), Some(nm));
        // At the root, or in node_modules itself, nothing is inherited.
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        assert_eq!(app.inherited_category(), None);
        app.go_to(&["node_modules".into()]);
        assert_eq!(app.inherited_category(), None);
    }

    /// A folder replaced by a symlink or a file is not rescanned alone.
    #[cfg(unix)]
    #[test]
    fn subtree_rejects_non_directories() {
        let root = temp("nondir");
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("d")).unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("link")).unwrap();
        fs::write(root.join("f.txt"), "x").unwrap();
        let sub = |p: &str| scan_subtree(&root.join(p), true, true, &Progress::default(), None, None);
        assert!(sub("d").is_ok());
        assert!(sub("link").is_err());
        assert!(sub("f.txt").is_err());
        assert!(!crate::scan::is_real_dir(&root.join("link")));
    }

    /// With one_fs, a folder now on another device than the scan root is
    /// refused (simulated with a wrong root device id).
    #[cfg(unix)]
    #[test]
    fn subtree_rejects_other_filesystem() {
        let root = temp("mount");
        fs::create_dir_all(root.join("d")).unwrap();
        let dev = crate::scan::device_of(&root).unwrap();
        let sub = |dev| scan_subtree(&root.join("d"), true, true, &Progress::default(), None, Some(dev));
        assert!(sub(dev).is_ok());
        assert!(sub(dev.wrapping_add(1)).is_err());
        // --cross-fs: no device check.
        assert!(scan_subtree(&root.join("d"), false, true, &Progress::default(), None, Some(dev + 1)).is_ok());
    }

    /// Non-UTF-8 names (allowed on Linux) are scanned, not reported as
    /// unreadable, and such a cache directory is still excluded.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_are_scanned_and_excluded() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let root = temp("nonutf8");
        let weird = root.join(OsStr::from_bytes(b"d\xffir"));
        fs::create_dir_all(&weird).unwrap();
        fs::write(weird.join("f.bin"), vec![0u8; 50_000]).unwrap();
        let node = full(&root);
        assert_eq!(node.errors(), 0);
        assert_eq!(node.files(), 1);
        let excluded = scan_excluding(&root, true, true, &Progress::default(), Some(&weird)).unwrap();
        assert_eq!((excluded.files(), excluded.errors()), (0, 0));
        // TUI paths for filesystem operations resolve the raw name.
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        let lossy = app.root.children().iter().find(|c| c.lossy).unwrap();
        app.go_to(&[Step::of(lossy)]);
        assert_eq!(app.fs_path(), Some(weird.clone()));
        assert_ne!(app.current_path(), weird);
        assert!(app.path_has_lossy_name());

        // A UTF-8 sibling spelled like the escaped rendering must not be
        // picked, nor a second invalid name (distinct escapes).
        let twin = root.join("d\\xffir");
        let other = root.join(OsStr::from_bytes(b"d\xfeir"));
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&twin).unwrap();
        let app = {
            let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
            let i = app
                .root
                .children()
                .iter()
                .position(|c| c.lossy && &*c.name == "d\\xffir")
                .unwrap();
            let name = app.root.children()[i].name.clone();
            assert!(app.root.children().iter().any(|c| !c.lossy && c.name == name));
            app.states[0].select(Some(i));
            app.enter();
            app
        };
        assert_eq!(app.fs_path(), Some(weird.clone()));

        // Going back to a location picks the sibling with the same lossiness,
        // in either direction.
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        let utf8 = app.root.children().iter().find(|c| !c.lossy && c.is_dir()).unwrap();
        let to = |name: &str| {
            let n = app
                .root
                .children()
                .iter()
                .find(|c| c.lossy && &*c.name == name)
                .unwrap();
            [Step::of(n)]
        };
        let (to_utf8, to_ff, to_fe) = ([Step::of(utf8)], to("d\\xffir"), to("d\\xfeir"));
        app.go_to(&to_utf8);
        assert_eq!(app.fs_path(), Some(twin.clone()));
        for (steps, want) in [(&to_ff, &weird), (&to_fe, &other)] {
            let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
            app.go_to(steps);
            assert_eq!(app.fs_path().as_ref(), Some(want));
        }

        // Popup results keep the raw path too (the twin is empty, so the
        // only file in the biggest category is the one in the raw dir).
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.open_popup();
        let popup = app.popup.as_ref().unwrap();
        assert_eq!(popup.items[0].fs_path, Some(weird.join("f.bin")));
    }

    /// An ancestor replaced by a symlink makes the checked path unavailable,
    /// so `r` falls back to a full rescan.
    #[cfg(unix)]
    #[test]
    fn ancestor_symlink_blocks_subtree_rescan() {
        let root = temp("ancestor");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("elsewhere/b")).unwrap();
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.go_to(&["a".into(), "b".into()]);
        assert_eq!(app.fs_path_checked(), Some(root.join("a/b")));
        fs::rename(root.join("a"), root.join("a-old")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("a")).unwrap();
        assert!(crate::scan::is_real_dir(&root.join("a/b"))); // the leaf alone looks fine
        assert_eq!(app.fs_path_checked(), None);
    }

    /// APFS clones: `r` keeps families that span the rescanned folder and
    /// the rest of the tree, and `c` re-sorts by deduplicated size.
    #[cfg(target_os = "macos")]
    #[test]
    fn clones_survive_rescan_and_sort_by_dedup() {
        use std::process::Command;
        let root = temp("clones");
        fs::create_dir_all(root.join("copies")).unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::write(root.join("copies/a"), vec![1u8; 4 << 20]).unwrap();
        for c in ["copies/b", "orig"] {
            let ok = Command::new("cp")
                .arg("-c")
                .arg(root.join("copies/a"))
                .arg(root.join(c))
                .status()
                .unwrap();
            assert!(ok.success());
        }
        fs::write(root.join("plain/p"), vec![2u8; 6 << 20]).unwrap();
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);

        // Without --clones, `c` only says how to get dedup sizes, and what
        // deleting frees is unknown on macOS.
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.size_mode, SizeMode::Du);
        assert!(app.status.as_deref().unwrap().contains("--clones"));
        assert_eq!(app.freeable(&app.root), None);
        (app.clones, app.clone_ids) = (true, true);
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.size_mode, SizeMode::Dedup);
        assert_eq!(app.freeable(&app.root), Some(10 << 20));
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.size_mode, SizeMode::Du);
        let names = |app: &App| {
            app.root
                .children()
                .iter()
                .map(|c| c.name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["copies", "plain", "orig"]);
        // "orig" (shortest path) keeps the family's bytes.
        assert_eq!(app.root.dedup(), 10 << 20);
        assert_eq!(app.root.freeable(), 10 << 20);
        assert_eq!(app.root.children()[0].dedup(), 0);
        assert_eq!(app.root.children()[0].freeable(), 0);

        app.set_size_mode(SizeMode::Dedup);
        assert_eq!(names(&app), ["plain", "orig", "copies"]);
        app.set_size_mode(SizeMode::Du);
        assert_eq!(names(&app), ["copies", "plain", "orig"]);

        // Caches are written in du order, even from dedup mode, and the
        // cursor stays put.
        app.set_size_mode(SizeMode::Dedup);
        app.states[0].select(Some(2));
        let inside = app.in_du_order(|root| root.children().iter().map(|c| c.name.to_string()).collect::<Vec<_>>());
        assert_eq!(inside, ["copies", "plain", "orig"]);
        assert_eq!(names(&app), ["plain", "orig", "copies"]);
        assert_eq!(app.states[0].selected(), Some(2));

        // A category popup ranks and shows dedup sizes too: the copies in
        // "copies" add nothing once "orig" holds the family.
        app.focus = Focus::Breakdown;
        app.breakdown_state.select(Some(0));
        app.open_popup();
        let sizes = |app: &App| {
            app.popup
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|i| i.size)
                .collect::<Vec<_>>()
        };
        assert_eq!(sizes(&app), [6 << 20, 4 << 20, 0, 0]);
        assert_eq!(app.popup.as_ref().unwrap().items[1].path, Path::new("orig"));
        app.popup = None;
        app.set_size_mode(SizeMode::Du);
        app.open_popup();
        assert_eq!(sizes(&app), [6 << 20, 4 << 20, 4 << 20, 4 << 20]);
        app.popup = None;
        app.focus = Focus::Entries;

        // Rescan "copies" after a third clone appears there.
        let ok = Command::new("cp")
            .arg("-c")
            .arg(root.join("copies/a"))
            .arg(root.join("copies/c"))
            .status()
            .unwrap();
        assert!(ok.success());
        app.set_size_mode(SizeMode::Dedup);
        app.go_to(&["copies".into()]);
        let progress = Progress::default();
        let fresh = scan_subtree(&app.fs_path().unwrap(), true, true, &progress, None, None).unwrap();
        // A rescan that sees clone flags makes the tree's detection known
        // (here as if the first scan had seen none).
        app.clone_ids = false;
        let saw = |f: &std::sync::atomic::AtomicBool| f.load(std::sync::atomic::Ordering::Relaxed);
        app.replace_current(fresh, saw(&progress.clone_ids));
        assert!(!app.clone_gaps);

        // Gaps follow the tree: a subtree that had files of unknown clone
        // state, rescanned with none, leaves no gap behind.
        node_at_mut(&mut app.root, &app.stack)
            .dir
            .as_mut()
            .unwrap()
            .clones_mut()
            .gaps = 2;
        app.root.dir.as_mut().unwrap().clones_mut().gaps = 2;
        app.clone_gaps = true;
        let fresh = scan_subtree(&app.fs_path().unwrap(), true, true, &Progress::default(), None, None).unwrap();
        app.replace_current(fresh, true);
        assert!(!app.clone_gaps);
        assert_eq!(app.root.clone_gaps(), 0);
        assert!(app.clone_ids);
        assert_eq!(app.root.dedup(), 10 << 20);
        assert_eq!(app.current().name.as_ref(), "copies");
        assert_eq!(names(&app), ["plain", "orig", "copies"]);
        app.set_size_mode(SizeMode::Du);
        same(&app.root, &full(&root));
        fs::remove_dir_all(&root).unwrap();
    }

    /// In dedup mode, a rescan that removes the last clone below an
    /// ancestor still re-sorts that ancestor by its new sizes.
    #[cfg(target_os = "macos")]
    #[test]
    fn dedup_rescan_resorts_ancestors_without_clones() {
        let root = temp("dedup-resort");
        fs::create_dir_all(root.join("p")).unwrap();
        fs::create_dir_all(root.join("q")).unwrap();
        fs::write(root.join("p/x"), vec![1u8; 4 << 20]).unwrap();
        let ok = std::process::Command::new("cp")
            .arg("-c")
            .arg(root.join("p/x"))
            .arg(root.join("p/y"))
            .status()
            .unwrap();
        assert!(ok.success());
        fs::write(root.join("q/z"), vec![2u8; 6 << 20]).unwrap();
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        (app.clones, app.clone_ids) = (true, true);
        app.set_size_mode(SizeMode::Dedup);
        let names = |app: &App| {
            app.root
                .children()
                .iter()
                .map(|c| c.name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["q", "p"]);

        // x and y become plain files (EF_MAY_SHARE_BLOCKS is sticky, so both
        // are written anew): p (8 MiB, no clones) now beats q (6 MiB).
        for f in ["p/x", "p/y"] {
            fs::remove_file(root.join(f)).unwrap();
            fs::write(root.join(f), vec![3u8; 4 << 20]).unwrap();
        }
        app.go_to(&["p".into()]);
        let fresh = scan_subtree(&app.fs_path().unwrap(), true, true, &Progress::default(), None, None).unwrap();
        app.replace_current(fresh, true);
        assert!(!app.root.clone);
        assert_eq!(names(&app), ["p", "q"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn base64() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64("/路徑".as_bytes()), "L+i3r+W+kQ==");
    }
}
