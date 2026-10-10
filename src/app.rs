use crate::category::{self, Category, by_size, reg};
use crate::scan::{Node, TopN, walk};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Color;
use ratatui::widgets::TableState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    pub color: Color,
    pub size: u64,
    pub count: Option<u64>,
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
    pub errors: u64,
    pub quit: bool,
}

pub fn node_at<'a>(root: &'a Node, stack: &[usize]) -> &'a Node {
    stack.iter().fold(root, |n, &i| &n.children()[i])
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
            errors,
            quit: false,
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
                        color: c.color(),
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
                    let path = self.current_path().join(&*self.current().children()[i].name);
                    reveal(&path);
                }
            }
            _ => {}
        }
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
                    reveal(&popup.base.join(&popup.items[i].0));
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
        self.popup = Some(Popup { title, base: self.current_path(), items, state });
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
            let e = map.entry(ext_key(&n.name)).or_insert((0, 0, reg().classify_file(&n.name)));
            e.0 += n.size;
            e.1 += 1;
        }
        true
    });
    let mut items: Vec<_> = map
        .into_iter()
        .map(|(ext, (size, count, cat))| BreakdownItem {
            label: if ext == "(none)" { ext.clone() } else { format!(".{ext}") },
            key: Key::Ext(ext),
            color: cat.color(),
            size,
            count: Some(count),
        })
        .collect();
    items.sort_by(|a, b| b.size.cmp(&a.size));
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
    let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}
