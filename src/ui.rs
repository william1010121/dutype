use crate::app::{App, Focus, Mode, node_at};
use crate::category::{ALL, COUNT, Category};
use crate::scan::Node;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table};
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
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
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
fn apportion(cats: &[u64; COUNT], len: usize) -> Vec<(Category, usize)> {
    let total: u64 = cats.iter().sum();
    if total == 0 || len == 0 {
        return Vec::new();
    }
    let mut parts: Vec<(Category, usize, f64)> = ALL
        .iter()
        .filter(|c| cats[c.index()] > 0)
        .map(|&c| {
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
fn stacked_bar(cats: &[u64; COUNT], size: u64, max: u64, width: u16, glyph: &str) -> Line<'static> {
    let len = (ratio(size, max) * width as f64).round() as usize;
    let len = if size > 0 { len.clamp(1, width as usize) } else { 0 };
    let mut spans: Vec<Span> = apportion(cats, len)
        .into_iter()
        .map(|(c, n)| Span::styled(glyph.repeat(n), Style::new().fg(c.color())))
        .collect();
    spans.push(Span::styled("·".repeat(width as usize - len), Style::new().fg(DIM)));
    Line::from(spans)
}

fn dominant(cats: &[u64; COUNT]) -> Category {
    *ALL.iter().max_by_key(|c| cats[c.index()]).unwrap()
}

fn panel(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { BORDER }))
        .title(title)
        .padding(Padding::horizontal(1))
}

fn panel_title(text: &str, focused: bool) -> Line<'static> {
    let style = if focused { Style::new().fg(ACCENT).bold() } else { Style::new().fg(MUTED).bold() };
    Line::from(Span::styled(format!(" {text} "), style))
}

fn chip(label: &str, bg: Color) -> Span<'static> {
    Span::styled(format!(" {label} "), Style::new().fg(Color::Black).bg(bg).bold())
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
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
    let [breakdown, details] = Layout::vertical([Constraint::Min(6), Constraint::Length(10)]).areas(right);

    draw_header(f, app, header);
    draw_entries(f, app, left);
    draw_breakdown(f, app, breakdown);
    draw_details(f, app, details);

    let keys: &[(&str, &str)] = if app.popup.is_some() {
        &[("↑↓", "move"), ("⏎ o", "reveal"), ("esc", "close")]
    } else {
        &[
            ("↑↓", "move"),
            ("⏎ →", "open"),
            ("← ⌫", "up"),
            ("tab", "pane"),
            ("m", "category/ext"),
            ("o", "reveal"),
            ("q", "quit"),
        ]
    };
    f.render_widget(Paragraph::new(keycaps(keys)), footer);

    if app.popup.is_some() {
        draw_popup(f, app);
    }
}

/// `a › b › c`, dropping leading segments with `…` when it does not fit.
fn breadcrumb(path: &Path, width: usize) -> Line<'static> {
    let segs: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let sep = || Span::styled(" › ", Style::new().fg(DIM));
    let build = |from: usize| {
        let mut spans = vec![Span::styled(if from > 0 { "…" } else { "/" }, Style::new().fg(MUTED))];
        for (i, s) in segs.iter().enumerate().skip(from) {
            spans.push(sep());
            let style = if i + 1 == segs.len() { Style::new().fg(ACCENT).bold() } else { Style::new().fg(MUTED) };
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
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![Span::raw(" "), chip("◆ dutype", ACCENT), Span::raw(" ")]))
        .title(
            Line::from(vec![
                Span::styled(human(cur.size), size_style(cur.size)),
                Span::styled(format!("  ·  {} files  ·  scanned in {:.2}s", thousands(cur.files()), app.scan_time.as_secs_f64()), Style::new().fg(MUTED)),
                Span::styled(
                    if app.errors > 0 { format!("  ·  {} unreadable ", thousands(app.errors)) } else { " ".into() },
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
    let mut items: Vec<_> = ALL.iter().filter(|c| cats[c.index()] > 0).collect();
    items.sort_by(|a, b| cats[b.index()].cmp(&cats[a.index()]));
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
    let dot = Span::styled("● ", Style::new().fg(dominant(&cats).color()));
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
        Line::from(vec![dot, Span::styled(c.name.to_string(), Style::new().fg(Color::Indexed(250)))])
    }
}

fn draw_entries(f: &mut Frame, app: &mut App, area: Rect) {
    let App { root, stack, states, focus, .. } = app;
    let cur = node_at(root, stack);
    let focused = *focus == Focus::Entries;
    let block = panel(panel_title("Entries", focused), focused).title(
        Line::from(Span::styled(format!(" {} items ", thousands(cur.children().len() as u64)), Style::new().fg(MUTED)))
            .right_aligned(),
    );

    if cur.children().is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [mid] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(inner);
        f.render_widget(Paragraph::new("empty directory").fg(MUTED).alignment(Alignment::Center), mid);
        return;
    }

    let max = cur.children().first().map_or(1, |c| c.size);
    let rows = cur.children().iter().map(|c| {
        Row::new(vec![
            Cell::from(Line::from(Span::styled(human(c.size), size_style(c.size))).right_aligned()),
            Cell::from(Span::styled(pct(c.size, cur.size), Style::new().fg(MUTED))),
            Cell::from(stacked_bar(&c.cats(), c.size, max, ENTRY_BAR, "█")),
            Cell::from(entry_name(c)),
        ])
    });
    let table = Table::new(
        rows,
        [Constraint::Length(10), Constraint::Length(6), Constraint::Length(ENTRY_BAR), Constraint::Fill(1)],
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
            Span::styled(format!(" {label} "), Style::new().fg(Color::Black).bg(if focused { ACCENT } else { MUTED }).bold())
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
                Span::styled("● ", Style::new().fg(b.color)),
                Span::styled(b.label.clone(), Style::new().fg(TEXT)),
            ])),
            Cell::from(Span::styled(smooth_bar(ratio(b.size, max), BREAKDOWN_BAR), Style::new().fg(b.color))),
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
        .block(panel(title, focused).title(
            Line::from(Span::styled(" ⏎ list files ", Style::new().fg(if focused { MUTED } else { DIM }))).right_aligned(),
        ))
        .row_highlight_style(highlight())
        .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(ACCENT))));
    f.render_stateful_widget(table, area, &mut app.breakdown_state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let block = panel(panel_title("Selected", false), false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cur = app.current();
    let Some(node) = app.states.last().and_then(|s| s.selected()).and_then(|i| cur.children().get(i)) else {
        f.render_widget(Paragraph::new("nothing selected").fg(MUTED), inner);
        return;
    };
    let cats = node.cats();
    let label = |s: &str| Span::styled(format!("{s:<8}"), Style::new().fg(MUTED));

    let mut lines = vec![
        entry_name(node),
        Line::from(vec![
            label("size"),
            Span::styled(human(node.size), size_style(node.size)),
            Span::styled(format!("   {} of parent", pct(node.size, cur.size).trim()), Style::new().fg(MUTED)),
        ]),
    ];
    if node.is_dir() {
        lines.push(Line::from(vec![label("files"), Span::styled(thousands(node.files()), Style::new().fg(TEXT))]));
    } else {
        lines.push(Line::from(vec![
            label("type"),
            Span::styled(node.cat.label(), Style::new().fg(node.cat.color())),
        ]));
    }
    lines.push(stacked_bar(&cats, node.size, node.size, inner.width, "▀"));

    let mut items: Vec<_> = ALL.iter().filter(|c| cats[c.index()] > 0).collect();
    items.sort_by(|a, b| cats[b.index()].cmp(&cats[a.index()]));
    let rows_left = (inner.height as usize).saturating_sub(lines.len());
    let col = (inner.width as usize / 2).max(1);
    for pair in items.chunks(2).take(rows_left) {
        let mut spans = Vec::new();
        for c in pair {
            let text = format!("{} {} {}", c.label(), human(cats[c.index()]), pct(cats[c.index()], node.size).trim());
            spans.push(Span::styled("● ", Style::new().fg(c.color())));
            spans.push(Span::styled(format!("{text:<w$}", w = col.saturating_sub(2)), Style::new().fg(MUTED)));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_popup(f: &mut Frame, app: &mut App) {
    let popup = app.popup.as_mut().unwrap();
    let [area] = Layout::horizontal([Constraint::Percentage(86)]).flex(Flex::Center).areas(f.area());
    let [area] = Layout::vertical([Constraint::Percentage(82)]).flex(Flex::Center).areas(area);
    f.render_widget(Clear, area);

    let total: u64 = popup.items.iter().map(|i| i.1).sum();
    let max = popup.items.first().map_or(1, |i| i.1);
    let rows = popup.items.iter().enumerate().map(|(i, (p, s))| {
        let parent = p.parent().map(|d| d.display().to_string()).filter(|d| !d.is_empty());
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut path = Vec::new();
        if let Some(d) = parent {
            path.push(Span::styled(format!("{d}/"), Style::new().fg(MUTED)));
        }
        path.push(Span::styled(name, Style::new().fg(TEXT).bold()));
        Row::new(vec![
            Cell::from(Line::from(Span::styled(format!("{}", i + 1), Style::new().fg(DIM))).right_aligned()),
            Cell::from(Line::from(Span::styled(human(*s), size_style(*s))).right_aligned()),
            Cell::from(Span::styled(smooth_bar(ratio(*s, max), 12), Style::new().fg(ACCENT))),
            Cell::from(Line::from(path)),
        ])
    });
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![Span::raw(" "), chip(&popup.title, ACCENT), Span::raw(" ")]))
        .title(
            Line::from(Span::styled(
                format!(" top {} · {} ", thousands(popup.items.len() as u64), human(total)),
                Style::new().fg(MUTED),
            ))
            .right_aligned(),
        )
        .title_bottom(keycaps(&[("↑↓", "move"), ("⏎ o", "reveal"), ("esc", "close")]))
        .padding(Padding::new(1, 1, 1, 0));
    let table = Table::new(
        rows,
        [Constraint::Length(4), Constraint::Length(10), Constraint::Length(12), Constraint::Fill(1)],
    )
    .column_spacing(2)
    .block(block)
    .row_highlight_style(highlight())
    .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(ACCENT))));
    f.render_stateful_widget(table, area, &mut popup.state);
}

pub fn draw_scanning(f: &mut Frame, path: &Path, files: u64, bytes: u64, elapsed: Duration) {
    let [area] = Layout::horizontal([Constraint::Length(64)]).flex(Flex::Center).areas(f.area());
    let [area] = Layout::vertical([Constraint::Length(9)]).flex(Flex::Center).areas(area);
    let frame = SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()];
    let secs = elapsed.as_secs_f64().max(0.001);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![Span::raw(" "), chip("◆ dutype", ACCENT), Span::raw(" ")]))
        .title_bottom(Line::from(keycaps(&[("q", "abort")])).right_aligned())
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
            Span::styled(path.display().to_string(), Style::new().fg(ACCENT)),
        ]),
        Line::default(),
        stat("files", format!("{}  ({}/s)", thousands(files), thousands((files as f64 / secs) as u64))),
        stat("size", human(bytes)),
        stat("elapsed", format!("{secs:.1}s")),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}
