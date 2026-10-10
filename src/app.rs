use crate::category::{self, Category, by_size, reg};
use crate::scan::{Node, TopN, walk};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
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

pub struct Popup {
    pub title: String,
    /// Paths relative to the directory the popup was opened in.
    pub base: PathBuf,
    pub items: Vec<(PathBuf, u64)>,
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

    /// The current folder's real path on disk, for filesystem operations
    /// (rescan, copy, reveal); [`Self::current_path`] is for display.
    pub fn fs_path(&self) -> PathBuf {
        let mut p = self.root_path.clone();
        let mut n = &self.root;
        for &i in &self.stack {
            n = &n.children()[i];
            p = crate::scan::resolve_child(&p, &n.name);
        }
        p
    }

    /// Names from the root to the current directory.
    pub fn location(&self) -> Vec<Box<str>> {
        let mut n = &self.root;
        self.stack
            .iter()
            .map(|&i| {
                n = &n.children()[i];
                n.name.clone()
            })
            .collect()
    }

    /// Descend into the directories named by `location` (as far as they exist).
    pub fn go_to(&mut self, location: &[Box<str>]) {
        for name in location {
            let Some(i) = self
                .current()
                .children()
                .iter()
                .position(|c| c.name == *name && c.is_dir())
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
    pub fn replace_current(&mut self, mut fresh: Node) {
        let location = self.location();
        // Selected entry name at every level, to restore after re-sorting.
        let selected: Vec<Option<Box<str>>> = (0..=self.stack.len())
            .map(|k| {
                let node = node_at(&self.root, &self.stack[..k]);
                self.states[k]
                    .selected()
                    .and_then(|i| node.children().get(i))
                    .map(|c| c.name.clone())
            })
            .collect();

        let target = node_at_mut(&mut self.root, &self.stack);
        fresh.name = target.name.clone();
        let old = std::mem::replace(target, fresh);
        let new = node_at(&self.root, &self.stack);
        let size_delta = new.size as i128 - old.size as i128;
        let files_delta = new.files() as i128 - old.files() as i128;
        let errors_delta = new.errors() as i128 - old.errors() as i128;
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
                dir.cats.iter_mut().zip(&cats_delta).for_each(|(c, d)| shift(c, *d));
                dir.children
                    .sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
            }
        }
        // The subtree's old errors were replaced, not added to.
        self.errors = self.root.errors();
        // Free the old subtree off the UI thread; it can be large.
        std::thread::spawn(move || drop(old));

        self.stack.clear();
        self.states = vec![first_selected(self.root.children().len())];
        self.go_to(&location);
        for (k, name) in selected.iter().enumerate().take(self.states.len()) {
            let node = node_at(&self.root, &self.stack[..k]);
            let pos = name
                .as_ref()
                .and_then(|n| node.children().iter().position(|c| c.name == *n));
            self.states[k].select(pos.or(if node.children().is_empty() { None } else { Some(0) }));
        }
        self.popup = None;
        self.refresh_breakdown();
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
            KeyCode::Char('m') => {
                self.mode = match self.mode {
                    Mode::Category => Mode::Extension,
                    Mode::Extension => Mode::Category,
                };
                self.refresh_breakdown();
            }
            KeyCode::Char('o') => {
                if let Some(i) = self.selected_entry() {
                    let path = crate::scan::resolve_child(&self.fs_path(), &self.current().children()[i].name);
                    reveal(&path);
                }
            }
            KeyCode::Char('y') => {
                // The selected entry, or the current folder from the breakdown pane.
                let path = match (self.focus, self.selected_entry()) {
                    (Focus::Entries, Some(i)) => {
                        crate::scan::resolve_child(&self.fs_path(), &self.current().children()[i].name)
                    }
                    _ => self.fs_path(),
                };
                self.copy(&path);
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
        self.status = Some(match copy_to_clipboard(&path.display().to_string()) {
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
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(i) = popup.state.selected() {
                    reveal(&crate::scan::resolve_relative(&popup.base, &popup.items[i].0));
                }
            }
            KeyCode::Char('y') => {
                if let Some(i) = popup.state.selected() {
                    let path = popup.base.join(&popup.items[i].0);
                    self.copy(&path);
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

    fn open_popup(&mut self) {
        let Some(item) = self.breakdown_state.selected().and_then(|i| self.breakdown.get(i)) else {
            return;
        };
        let cur = self.current();
        let mut top = TopN::new(POPUP_LIMIT);
        let mut push = |size: u64, path: &Path| {
            if top.accepts(|min: &(u64, PathBuf)| size > min.0) {
                top.push((size, path.to_path_buf()));
            }
        };
        let title;
        match &item.key {
            // For directory kinds, list the special directories themselves.
            Key::Cat(c) if c.is_dir_kind() => {
                title = format!("{} directories", c.label());
                walk(cur, &mut PathBuf::new(), &mut |n, p| {
                    if n.special && n.cat == *c {
                        push(n.size, p);
                        return false;
                    }
                    true
                });
            }
            Key::Cat(c) => {
                title = format!("{} files", c.label());
                walk(cur, &mut PathBuf::new(), &mut |n, p| {
                    if !n.is_dir() && n.cat == *c {
                        push(n.size, p);
                    }
                    true
                });
            }
            Key::Ext(e) => {
                title = format!("*.{e} files");
                walk(cur, &mut PathBuf::new(), &mut |n, p| {
                    if !n.is_dir() && ext_key(&n.name) == *e {
                        push(n.size, p);
                    }
                    true
                });
            }
        }
        let items: Vec<_> = top.into_sorted().into_iter().map(|(s, p)| (p, s)).collect();
        let state = first_selected(items.len());
        self.popup = Some(Popup {
            title,
            base: self.fs_path(),
            items,
            state,
        });
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
    items.sort_by_key(|i| std::cmp::Reverse(i.size));
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
        scan_excluding(root, true, &Progress::default(), None).unwrap()
    }

    fn same(a: &Node, b: &Node) {
        assert_eq!((&a.name, a.size, a.cat, a.special), (&b.name, b.size, b.cat, b.special));
        assert_eq!((a.files(), a.cats()), (b.files(), b.cats()), "{}", a.name);
        let names = |n: &Node| n.children().iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(a), names(b), "order under {}", a.name);
        a.children().iter().zip(b.children()).for_each(|(x, y)| same(x, y));
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
        assert_eq!(app.location(), vec![Box::from("small"), Box::from("inner")]);

        // "small" grows past "big", so the root's order has to change.
        fs::write(root.join("small/inner/c.zip"), vec![0u8; 900_000]).unwrap();
        let fresh = scan_subtree(
            &app.current_path(),
            true,
            &Progress::default(),
            app.inherited_category(),
            None,
        )
        .unwrap();
        app.replace_current(fresh);

        same(&app.root, &full(&root));
        assert_eq!(app.location(), vec![Box::from("small"), Box::from("inner")]);
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
            &Progress::default(),
            app.inherited_category(),
            None,
        )
        .unwrap();
        app.replace_current(fresh);
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
                &Progress::default(),
                app.inherited_category(),
                None,
            )
            .unwrap()
        };
        for _ in 0..3 {
            let fresh = rescan(&app);
            app.replace_current(fresh);
            assert_eq!(app.errors, 1);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let fresh = rescan(&app);
        app.replace_current(fresh);
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
        let sub = |p: &str| scan_subtree(&root.join(p), true, &Progress::default(), None, None).unwrap();
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
        let node = scan_excluding(&root, true, &progress, None).unwrap();
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
        let sub = |p: &str| scan_subtree(&root.join(p), true, &Progress::default(), None, None);
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
        let sub = |dev| scan_subtree(&root.join("d"), true, &Progress::default(), None, Some(dev));
        assert!(sub(dev).is_ok());
        assert!(sub(dev.wrapping_add(1)).is_err());
        // --cross-fs: no device check.
        assert!(scan_subtree(&root.join("d"), false, &Progress::default(), None, Some(dev + 1)).is_ok());
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
        let excluded = scan_excluding(&root, true, &Progress::default(), Some(&weird)).unwrap();
        assert_eq!((excluded.files(), excluded.errors()), (0, 0));
        // TUI paths for filesystem operations resolve the raw name.
        let mut app = App::new(full(&root), root.clone(), Duration::ZERO, 0);
        app.go_to(&[app.root.children()[0].name.clone()]);
        assert_eq!(app.fs_path(), weird);
        assert_ne!(app.current_path(), weird);
        assert!(crate::scan::is_real_dir(&app.fs_path()));
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
