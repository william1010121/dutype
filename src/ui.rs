use crate::app::{App, Focus, Mode, node_at};
use crate::category::{Category, by_size, dominant, folder_icon};
use crate::report::SizeMode;
use crate::scan::Node;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table, TableState};
use std::path::Path;
use std::time::Duration;

// 256-color palette so it renders the same over SSH and in any terminal.
const ACCENT: Color = Color::Indexed(75);
const TEXT: Color = Color::Indexed(252);
const MUTED: Color = Color::Indexed(244);
const DIM: Color = Color::Indexed(238);
const BORDER: Color = Color::Indexed(240);
const SEL_BG: Color = Color::Indexed(236);
const KEY_BG: Color = Color::Indexed(238);

const ENTRY_BAR: u16 = 22;
const BREAKDOWN_BAR: u16 = 16;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// How long ago a unix time was, e.g. `3h ago`.
pub fn ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(unix_secs) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn ratio(part: u64, total: u64) -> f64 {
    part as f64 / total.max(1) as f64
}

fn pct(part: u64, total: u64) -> String {
    format!("{:5.1}%", ratio(part, total) * 100.0)
}

/// Sizes get brighter as they get bigger, so heavy rows stand out.
fn size_style(bytes: u64) -> Style {
    match bytes {
        b if b >= 10 << 30 => Style::new().fg(Color::Indexed(209)).bold(),
        b if b >= 1 << 30 => Style::new().fg(Color::Indexed(222)).bold(),
        b if b >= 1 << 20 => Style::new().fg(TEXT),
        _ => Style::new().fg(MUTED),
    }
}

/// Bar with 1/8-cell precision.
fn smooth_bar(frac: f64, width: u16) -> String {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let eighths = if frac > 0.0 { eighths.max(1) } else { 0 };
    format!("{}{}", "█".repeat(eighths / 8), PARTS[eighths % 8])
}

/// Splits `len` cells among categories proportionally (largest remainder),
/// biggest category first.
fn apportion(cats: &[u64], len: usize) -> Vec<(Category, usize)> {
    let total: u64 = cats.iter().sum();
    if total == 0 || len == 0 {
        return Vec::new();
    }
    let mut parts: Vec<(Category, usize, f64)> = by_size(cats)
        .into_iter()
        .map(|c| {
            let exact = cats[c.index()] as f64 * len as f64 / total as f64;
            (c, exact.floor() as usize, exact.fract())
        })
        .collect();
    let left = len - parts.iter().map(|p| p.1).sum::<usize>();
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by(|&a, &b| parts[b].2.total_cmp(&parts[a].2));
    for i in order.into_iter().take(left) {
        parts[i].1 += 1;
    }
    parts.sort_by(|a, b| cats[b.0.index()].cmp(&cats[a.0.index()]));
    parts.into_iter().filter(|p| p.1 > 0).map(|(c, n, _)| (c, n)).collect()
}

/// A bar whose length is `size / max` of `width`, made of colored segments
/// proportional to the category sizes, on a dim track.
fn stacked_bar(cats: &[u64], size: u64, max: u64, width: u16, glyph: &str) -> Line<'static> {
    let len = (ratio(size, max) * width as f64).round() as usize;
    let len = if size > 0 && width > 0 {
        len.clamp(1, width as usize)
    } else {
        0
    };
    let mut spans: Vec<Span> = apportion(cats, len)
        .into_iter()
        .map(|(c, n)| Span::styled(glyph.repeat(n), Style::new().fg(c.color())))
        .collect();
    spans.push(Span::styled("·".repeat(width as usize - len), Style::new().fg(DIM)));
    Line::from(spans)
}

fn panel(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { BORDER }))
        .title(title)
        .padding(Padding::horizontal(1))
}

fn panel_title(text: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED).bold()
    };
    Line::from(Span::styled(format!(" {text} "), style))
}

/// Approximate perceived brightness (0-255) of a palette color.
fn luminance(c: Color) -> u32 {
    const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r as u32, g as u32, b as u32),
        Color::Indexed(n @ 16..=231) => {
            let n = n as u32 - 16;
            (
                LEVELS[(n / 36) as usize],
                LEVELS[(n / 6 % 6) as usize],
                LEVELS[(n % 6) as usize],
            )
        }
        Color::Indexed(n @ 232..=255) => {
            let v = 8 + 10 * (n as u32 - 232);
            (v, v, v)
        }
        _ => (200, 200, 200),
    };
    (299 * r + 587 * g + 114 * b) / 1000
}

/// Icon (or a colored dot with `icons = "none"`) plus a space. Nerd Font glyphs
/// take the color; emoji keep their own.
fn icon_span(icon: Option<&'static str>, color: Color) -> Span<'static> {
    Span::styled(format!("{} ", icon.unwrap_or("●")), Style::new().fg(color))
}

fn chip(label: &str, bg: Color) -> Span<'static> {
    let fg = if luminance(bg) < 120 {
        Color::Indexed(255)
    } else {
        Color::Black
    };
    Span::styled(format!(" {label} "), Style::new().fg(fg).bg(bg).bold())
}

fn highlight() -> Style {
    Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD)
}

fn keycaps(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, label) in keys {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(TEXT).bg(KEY_BG).bold()));
        spans.push(Span::styled(format!(" {label}   "), Style::new().fg(MUTED)));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
    let [breakdown, details] = Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).areas(right);

    draw_header(f, app, header);
    draw_entries(f, app, left);
    draw_breakdown(f, app, breakdown);
    draw_details(f, app, details);

    let keys: &[(&str, &str)] = if app.popup.is_some() {
        &[
            ("s", "jump in TUI"),
            ("esc", "close"),
            ("↑↓", "move"),
            ("⏎ o", "reveal"),
            ("y", "copy path"),
        ]
    } else {
        &[
            ("↑↓", "move"),
            ("⏎ →", "open"),
            ("← ⌫", "up"),
            ("tab", "pane"),
            ("m", "category/ext"),
            ("c", "du/dedup"),
            ("o", "reveal"),
            ("y", "copy path"),
            ("r", "rescan folder"),
            ("R", "rescan all"),
            ("q", "quit"),
        ]
    };
    f.render_widget(Paragraph::new(keycaps(keys)), footer);
    if let Some(status) = &app.status {
        let line = Line::from(Span::styled(format!("{status} "), Style::new().fg(ACCENT).bold()));
        f.render_widget(Clear, footer);
        f.render_widget(Paragraph::new(line).right_aligned(), footer);
    }

    if app.popup.is_some() {
        draw_popup(f, app);
    }
    if app.show_keys
        && let Some((glyph, action)) = &app.last_key
    {
        draw_key_overlay(f, glyph, action);
    }
}

/// Screencast overlay with the last key pressed, bottom right.
fn draw_key_overlay(f: &mut Frame, glyph: &str, action: &str) {
    let mut spans = vec![Span::styled(
        format!(" {glyph} "),
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];
    if !action.is_empty() {
        spans.push(Span::styled(format!("  {action}"), Style::new().fg(TEXT).bold()));
    }
    let line = Line::from(spans);
    let area = f.area();
    let w = (line.width() as u16 + 4).min(area.width);
    let h = 3.min(area.height);
    let rect = Rect::new(
        area.right().saturating_sub(w + 2),
        area.bottom().saturating_sub(h + 2),
        w,
        h,
    );
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT));
    f.render_widget(Paragraph::new(line).block(block.padding(Padding::horizontal(1))), rect);
}

/// `a › b › c`, dropping leading segments with `…` when it does not fit.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(Into::into)
}

/// The path with the home directory shown as `~`.
pub fn display_path(path: &Path) -> String {
    match home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn breadcrumb(path: &Path, width: usize) -> Line<'static> {
    let home_rest = home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf));
    let root = if home_rest.is_some() { "~" } else { "/" };
    let path = home_rest.as_deref().unwrap_or(path);
    let segs: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let sep = || Span::styled(" › ", Style::new().fg(DIM));
    let build = |from: usize| {
        let mut spans = vec![Span::styled(if from > 0 { "…" } else { root }, Style::new().fg(MUTED))];
        for (i, s) in segs.iter().enumerate().skip(from) {
            spans.push(sep());
            let style = if i + 1 == segs.len() {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::new().fg(MUTED)
            };
            spans.push(Span::styled(s.clone(), style));
        }
        Line::from(spans)
    };
    (0..segs.len().max(1))
        .map(build)
        .find(|l| l.width() <= width)
        .unwrap_or_else(|| build(segs.len().saturating_sub(1)))
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cur = app.current();
    let total = app.shown_total(cur);
    let dedup = app.size_mode == SizeMode::Dedup;
    let mut left = vec![Span::raw(" "), chip("◆ dutype", ACCENT), Span::raw(" ")];
    if dedup {
        left.push(chip("dedup", Color::Indexed(117)));
        left.push(Span::raw(" "));
    }
    let shared = cur.size - cur.dedup();
    if shared > 0 {
        left.push(Span::styled(
            format!(
                " {}: −{} shared ",
                if app.reflink_fs.is_some() { "reflinks" } else { "clones" },
                human(shared)
            ),
            Style::new().fg(Color::Indexed(117)),
        ));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(left))
        .title(
            Line::from(vec![
                Span::styled(human(total), size_style(total)),
                Span::styled(
                    format!("  ·  {} files  ·  ", thousands(cur.files())),
                    Style::new().fg(MUTED),
                ),
                match crate::report::unix_secs(app.cached_at) {
                    Some(at) => Span::styled(format!("cached {}", ago(at)), Style::new().fg(Color::Indexed(222))),
                    None => Span::styled(
                        format!("scanned in {:.2}s", app.scan_time.as_secs_f64()),
                        Style::new().fg(MUTED),
                    ),
                },
                Span::styled(
                    if app.errors > 0 {
                        format!("  ·  {} unreadable ", thousands(app.errors))
                    } else {
                        " ".into()
                    },
                    Style::new().fg(Color::Indexed(203)),
                ),
            ])
            .right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let width = inner.width;
    let cats = cur.cats();
    let bar = stacked_bar(&cats, cur.size, cur.size, width, "█");

    let mut legend: Vec<Span> = Vec::new();
    let mut used = 0usize;
    let items = by_size(&cats);
    for c in items {
        let text = format!("{} {}  ", c.label(), pct(cats[c.index()], cur.size).trim());
        if used + text.chars().count() + 2 > width as usize {
            break;
        }
        used += text.chars().count() + 2;
        legend.push(Span::styled("● ", Style::new().fg(c.color())));
        legend.push(Span::styled(text, Style::new().fg(MUTED)));
    }

    let lines = vec![breadcrumb(&app.current_path(), width as usize), bar, Line::from(legend)];
    f.render_widget(Paragraph::new(lines), inner);
}

fn entry_name(c: &Node) -> Line<'static> {
    let cats = c.cats();
    let dot = if c.is_dir() && !c.special {
        icon_span(folder_icon(), dominant(&cats).color())
    } else {
        icon_span(c.cat.icon(), c.cat.color())
    };
    if c.is_dir() {
        let mut spans = vec![
            dot,
            Span::styled(c.name.to_string(), Style::new().fg(TEXT).bold()),
            Span::styled("/", Style::new().fg(MUTED)),
        ];
        if c.special {
            spans.push(Span::raw("  "));
            spans.push(chip(c.cat.label(), c.cat.color()));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            dot,
            Span::styled(c.name.to_string(), Style::new().fg(Color::Indexed(250))),
        ])
    }
}

/// Rows are one line high with no margins or table header/footer. Mirror
/// ratatui 0.29's fixed-height row bounds only to skip offscreen formatting;
/// the complete row list and real TableState still handle scrolling/rendering.
fn visible_entry_range(state: &TableState, len: usize, area: Rect) -> std::ops::Range<usize> {
    if len == 0 || area.is_empty() {
        return 0..0;
    }
    let height = usize::from(area.height);
    let mut start = state.offset().min(len - 1);
    if let Some(selected) = state.selected().map(|s| s.min(len - 1)) {
        if selected < start {
            start = selected;
        } else if selected >= start.saturating_add(height) {
            start = selected + 1 - height;
        }
    }
    start..start.saturating_add(height).min(len)
}

fn draw_entries(f: &mut Frame, app: &mut App, area: Rect) {
    // Shown size, and whether clone copies in it are counted elsewhere.
    let sizes: Vec<(u64, bool)> = {
        let cur = app.current();
        let copies = |i: usize| cur.dir.as_ref().is_some_and(|d| d.child_dedup(i) < d.children[i].size);
        (0..cur.children().len())
            .map(|i| (app.shown_size(cur, i), copies(i)))
            .collect()
    };
    let total = app.shown_total(app.current());
    let App {
        root,
        stack,
        states,
        focus,
        ..
    } = app;
    let cur = node_at(root, stack);
    let focused = *focus == Focus::Entries;
    let block = panel(panel_title("Entries", focused), focused).title(
        Line::from(Span::styled(
            format!(" {} items ", thousands(cur.children().len() as u64)),
            Style::new().fg(MUTED),
        ))
        .right_aligned(),
    );

    if cur.children().is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [mid] = Layout::vertical([Constraint::Length(1)])
            .flex(Flex::Center)
            .areas(inner);
        f.render_widget(
            Paragraph::new("empty directory").fg(MUTED).alignment(Alignment::Center),
            mid,
        );
        return;
    }

    let visible = visible_entry_range(states.last().unwrap(), cur.children().len(), block.inner(area));
    let max = sizes.first().map_or(1, |s| s.0);
    let rows = cur
        .children()
        .iter()
        .zip(&sizes)
        .enumerate()
        .map(|(i, (c, &(size, copies)))| {
            if !visible.contains(&i) {
                return Row::default().height(1);
            }
            let mut name = entry_name(c);
            // Holds (or is) a clone whose family is counted at another member.
            if copies {
                name.push_span(Span::styled("  ⧉", Style::new().fg(Color::Indexed(117))));
            }
            Row::new(vec![
                Cell::from(Line::from(Span::styled(human(size), size_style(size))).right_aligned()),
                Cell::from(Span::styled(pct(size, total), Style::new().fg(MUTED))),
                Cell::from(stacked_bar(&c.cats(), size, max, ENTRY_BAR, "█")),
                Cell::from(name),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(ENTRY_BAR),
            Constraint::Fill(1),
        ],
    )
    .column_spacing(2)
    .block(block)
    .row_highlight_style(highlight())
    .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(ACCENT))));
    f.render_stateful_widget(table, area, states.last_mut().unwrap());
}

fn draw_breakdown(f: &mut Frame, app: &mut App, area: Rect) {
    let total = app.current().size;
    let max = app.breakdown.first().map_or(1, |b| b.size);
    let ext_mode = app.mode == Mode::Extension;
    let focused = app.focus == Focus::Breakdown;

    let tab = |label: &str, active: bool| {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(Color::Black)
                    .bg(if focused { ACCENT } else { MUTED })
                    .bold(),
            )
        } else {
            Span::styled(format!(" {label} "), Style::new().fg(MUTED))
        }
    };
    let title = Line::from(vec![
        Span::raw(" "),
        tab("Category", !ext_mode),
        Span::styled("│", Style::new().fg(DIM)),
        tab("Extension", ext_mode),
        Span::raw(" "),
    ]);

    let rows = app.breakdown.iter().map(|b| {
        let mut cells = vec![
            Cell::from(Line::from(vec![
                icon_span(b.cat.icon(), b.cat.color()),
                Span::styled(b.label.clone(), Style::new().fg(TEXT)),
            ])),
            Cell::from(Span::styled(
                smooth_bar(ratio(b.size, max), BREAKDOWN_BAR),
                Style::new().fg(b.cat.color()),
            )),
            Cell::from(Span::styled(pct(b.size, total), Style::new().fg(MUTED))),
            Cell::from(Line::from(Span::styled(human(b.size), size_style(b.size))).right_aligned()),
        ];
        if ext_mode {
            cells.push(Cell::from(
                Line::from(Span::styled(thousands(b.count.unwrap_or(0)), Style::new().fg(MUTED))).right_aligned(),
            ));
        }
        Row::new(cells)
    });
    let mut widths = vec![
        Constraint::Fill(1),
        Constraint::Length(BREAKDOWN_BAR),
        Constraint::Length(6),
        Constraint::Length(10),
    ];
    if ext_mode {
        widths.push(Constraint::Length(8));
    }
    let table = Table::new(rows, widths)
        .block(
            panel(title, focused).title(
                Line::from(Span::styled(
                    // Category sizes stay allocated (du) sizes in dedup mode.
                    if app.size_mode == SizeMode::Dedup {
                        " du sizes · ⏎ list files "
                    } else {
                        " ⏎ list files "
                    },
                    Style::new().fg(if focused { MUTED } else { DIM }),
                ))
                .right_aligned(),
            ),
        )
        .row_highlight_style(highlight())
        .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(ACCENT))));
    f.render_stateful_widget(table, area, &mut app.breakdown_state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(panel_title("Selected", false), false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cur = app.current();
    let Some((i, node)) = app
        .states
        .last()
        .and_then(|s| s.selected())
        .and_then(|i| Some((i, cur.children().get(i)?)))
    else {
        f.render_widget(Paragraph::new("nothing selected").fg(MUTED), inner);
        return;
    };
    let dedup = cur.dir.as_ref().map_or(node.size, |d| d.child_dedup(i));
    let cats = node.cats();
    let label = |s: &str| Span::styled(format!("{s:<8}"), Style::new().fg(MUTED));

    let mut lines = vec![
        entry_name(node),
        Line::from(vec![
            label("size"),
            Span::styled(human(node.size), size_style(node.size)),
            Span::styled(
                format!("   {} of parent", pct(node.size, cur.size).trim()),
                Style::new().fg(MUTED),
            ),
        ]),
    ];
    // "freeable": what deleting it would free at least (shared blocks with
    // files elsewhere stay); "dedup": clones of each other counted once.
    let mut space = vec![
        label("frees"),
        Span::styled(
            // A file's freeable bytes can depend on its record in the parent.
            app.freeable(node)
                .map(|_| cur.dir.as_ref().map_or(0, |d| d.child_freeable(i)))
                .map_or("?".into(), |b| format!("≥ {}", human(b))),
            Style::new().fg(TEXT),
        ),
    ];
    if dedup != node.size {
        space.push(Span::styled(
            format!("   dedup {}", human(dedup)),
            Style::new().fg(Color::Indexed(117)),
        ));
    }
    lines.push(Line::from(space));
    // --private: what deleting this partial clone alone frees right now.
    let lossy = node.lossy || app.path_has_lossy_name();
    let private = (!node.is_dir() && !lossy && app.private_sizes && !app.private_missing)
        .then(|| app.privates.get(&app.current_rel().join(&*node.name)))
        .flatten();
    if let Some(p) = private {
        lines.push(Line::from(vec![
            label("private"),
            Span::styled(human(p), Style::new().fg(TEXT)),
            Span::styled(
                "   freed now if deleted (snapshots may keep it)",
                Style::new().fg(MUTED),
            ),
        ]));
    }
    if node.is_dir() {
        lines.push(Line::from(vec![
            label("files"),
            Span::styled(thousands(node.files()), Style::new().fg(TEXT)),
        ]));
    } else {
        lines.push(Line::from(vec![
            label("type"),
            Span::styled(node.cat.label(), Style::new().fg(node.cat.color())),
        ]));
    }
    lines.push(stacked_bar(&cats, node.size, node.size, inner.width, "▀"));

    let items = by_size(&cats);
    let rows_left = (inner.height as usize).saturating_sub(lines.len());
    let top = items.first().map_or(1, |c| cats[c.index()]);
    for c in items.iter().take(rows_left) {
        let v = cats[c.index()];
        let icon = icon_span(c.icon(), c.color());
        // icon + label (15) + bar + size (11) + pct (7)
        let bar_w = inner.width.saturating_sub(icon.width() as u16 + 15 + 11 + 7).max(4);
        lines.push(Line::from(vec![
            icon,
            Span::styled(format!("{:<15}", c.label()), Style::new().fg(TEXT)),
            Span::styled(
                format!("{:<w$}", smooth_bar(ratio(v, top), bar_w), w = bar_w as usize),
                Style::new().fg(c.color()),
            ),
            Span::styled(format!("{:>11}", human(v)), size_style(v)),
            Span::styled(format!("{:>7}", pct(v, node.size)), Style::new().fg(MUTED)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_popup(f: &mut Frame, app: &mut App) {
    let popup = app.popup.as_mut().unwrap();
    let [area] = Layout::horizontal([Constraint::Percentage(86)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::vertical([Constraint::Percentage(82)])
        .flex(Flex::Center)
        .areas(area);
    f.render_widget(Clear, area);

    let total: u64 = popup.items.iter().map(|i| i.size).sum();
    let max = popup.items.first().map_or(1, |i| i.size);
    let rows = popup.items.iter().enumerate().map(|(i, item)| {
        let (p, s) = (&item.path, item.size);
        let parent = p.parent().map(|d| d.display().to_string()).filter(|d| !d.is_empty());
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut path = Vec::new();
        if let Some(d) = parent {
            path.push(Span::styled(format!("{d}/"), Style::new().fg(MUTED)));
        }
        path.push(Span::styled(name, Style::new().fg(TEXT).bold()));
        Row::new(vec![
            Cell::from(Line::from(Span::styled(format!("{}", i + 1), Style::new().fg(DIM))).right_aligned()),
            Cell::from(Line::from(Span::styled(human(s), size_style(s))).right_aligned()),
            Cell::from(Span::styled(smooth_bar(ratio(s, max), 12), Style::new().fg(ACCENT))),
            Cell::from(Line::from(path)),
        ])
    });
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![
            Span::raw(" "),
            chip(&popup.title, ACCENT),
            Span::raw(" "),
        ]))
        .title(
            Line::from(Span::styled(
                format!(" top {} · {} ", thousands(popup.items.len() as u64), human(total)),
                Style::new().fg(MUTED),
            ))
            .right_aligned(),
        )
        .title_bottom(keycaps(&[
            ("s", "jump in TUI"),
            ("esc", "close"),
            ("↑↓", "move"),
            ("⏎ o", "reveal"),
            ("y", "copy"),
        ]))
        .padding(Padding::new(1, 1, 1, 0));
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Fill(1),
        ],
    )
    .column_spacing(2)
    .block(block)
    .row_highlight_style(highlight())
    .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(ACCENT))));
    f.render_stateful_widget(table, area, &mut popup.state);
}

pub fn draw_scanning(f: &mut Frame, path: &Path, files: u64, bytes: u64, elapsed: Duration) {
    let [area] = Layout::horizontal([Constraint::Length(64)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::vertical([Constraint::Length(9)]).flex(Flex::Center).areas(area);
    let frame = SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()];
    let secs = elapsed.as_secs_f64().max(0.001);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![
            Span::raw(" "),
            chip("◆ dutype", ACCENT),
            Span::raw(" "),
        ]))
        .title_bottom(keycaps(&[("q", "abort")]).right_aligned())
        .padding(Padding::new(2, 2, 1, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let stat = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<8}"), Style::new().fg(MUTED)),
            Span::styled(value, Style::new().fg(TEXT).bold()),
        ])
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{frame} "), Style::new().fg(ACCENT).bold()),
            Span::styled("Scanning ", Style::new().fg(TEXT).bold()),
            Span::styled(display_path(path), Style::new().fg(ACCENT)),
        ]),
        Line::default(),
        stat(
            "files",
            format!("{}  ({}/s)", thousands(files), thousands((files as f64 / secs) as u64)),
        ),
        stat("size", human(bytes)),
        stat("elapsed", format!("{secs:.1}s")),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod entry_viewport_tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::StatefulWidget;

    #[test]
    fn visible_range_keeps_global_offsets_and_empty_area_state() {
        let state = TableState::default().with_offset(99);
        assert_eq!(visible_entry_range(&state, 100, Rect::new(0, 0, 20, 8)), 99..100);
        assert_eq!(state.offset(), 99);
        let state = state.with_selected(Some(usize::MAX));
        assert_eq!(visible_entry_range(&state, 100, Rect::new(0, 0, 20, 8)), 99..100);
        assert_eq!(visible_entry_range(&state, 100, Rect::new(0, 0, 0, 8)), 0..0);
        assert_eq!(visible_entry_range(&state, 100, Rect::new(0, 0, 20, 0)), 0..0);
        assert_eq!(state.selected(), Some(usize::MAX));
        let state = TableState::default().with_selected(Some(99));
        assert_eq!(visible_entry_range(&state, 100, Rect::new(0, 0, 20, 8)), 92..100);
    }

    fn row(index: usize) -> Row<'static> {
        Row::new([
            Cell::from(format!("{index}")),
            Cell::from(format!("日本語-🦀-{index}")),
            Cell::from(Line::styled("multi\nline", Style::new().fg(ACCENT))),
            Cell::from("long invisible label".repeat(3)),
        ])
    }

    #[test]
    fn placeholders_match_pinned_table_buffers_and_global_state() {
        for len in [0usize, 1, 2, 24, 33, 100] {
            for (width, height) in [(0, 0), (2, 40), (4, 8), (6, 3), (12, 4), (40, 8), (80, 40)] {
                let area = Rect::new(0, 0, width, height);
                let positions = [
                    (None, 0),
                    (None, len.saturating_sub(1)),
                    (None, usize::MAX),
                    (Some(0), len + 1),
                    (Some(len.saturating_sub(1)), 0),
                    (Some(len + 1), len + 1),
                    (Some(usize::MAX), usize::MAX),
                ];
                for (selected, offset) in positions {
                    for column in [None, Some(0), Some(3), Some(usize::MAX)] {
                        let state = TableState::default()
                            .with_selected(selected)
                            .with_offset(offset)
                            .with_selected_column(column);
                        let block = panel(panel_title("Entries", true), true);
                        let visible = visible_entry_range(&state, len, block.inner(area));
                        let table = |rows| {
                            Table::new(
                                rows,
                                [
                                    Constraint::Length(4),
                                    Constraint::Length(12),
                                    Constraint::Length(8),
                                    Constraint::Fill(1),
                                ],
                            )
                            .column_spacing(2)
                            .block(block.clone())
                            .row_highlight_style(highlight())
                            .highlight_symbol("▌")
                        };
                        let full = table((0..len).map(row).collect::<Vec<_>>());
                        let windowed = table(
                            (0..len)
                                .map(|index| {
                                    if visible.contains(&index) {
                                        row(index)
                                    } else {
                                        Row::default().height(1)
                                    }
                                })
                                .collect::<Vec<_>>(),
                        );
                        let mut full_buffer = Buffer::empty(area);
                        let mut windowed_buffer = Buffer::empty(area);
                        let mut full_state = state.clone();
                        let mut windowed_state = state;
                        StatefulWidget::render(full, area, &mut full_buffer, &mut full_state);
                        StatefulWidget::render(windowed, area, &mut windowed_buffer, &mut windowed_state);
                        assert_eq!(
                            full_buffer, windowed_buffer,
                            "len={len}, area={area:?}, selected={selected:?}, offset={offset}, column={column:?}"
                        );
                        assert_eq!(full_state, windowed_state);
                    }
                }
            }
        }
    }
}
