mod app;
mod category;
mod scan;
mod ui;

use app::App;
use category::ALL;
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use scan::{Node, Progress};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Disk usage TUI that breaks folders down by file type and by special
/// directories (node_modules, .venv, .git, build output, caches, ...).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Directory to analyze
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Print a text report instead of opening the TUI
    #[arg(long)]
    report: bool,
    /// Descend into other filesystems / mount points
    #[arg(long)]
    cross_fs: bool,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let path = std::fs::canonicalize(&args.path)?;
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let handle = {
        let (path, progress, one_fs) = (path.clone(), progress.clone(), !args.cross_fs);
        std::thread::spawn(move || scan::scan(&path, one_fs, &progress))
    };

    if args.report {
        let root = handle.join().expect("scan thread panicked")?;
        print_report(&root, started.elapsed(), progress.errors.load(Ordering::Relaxed));
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = (|| {
        // Scanning screen.
        while !handle.is_finished() {
            terminal.draw(|f| {
                ui::draw_scanning(
                    f,
                    &path,
                    progress.files.load(Ordering::Relaxed),
                    progress.bytes.load(Ordering::Relaxed),
                    started.elapsed(),
                )
            })?;
            if event::poll(Duration::from_millis(80))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
                && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                return Ok(());
            }
        }
        let root = handle.join().expect("scan thread panicked")?;
        let mut app = App::new(root, path.clone(), started.elapsed(), progress.errors.load(Ordering::Relaxed));
        while !app.quit {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.handle_key(k);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn print_report(root: &Node, elapsed: Duration, errors: u64) {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{}  —  {}  ·  {} files  ·  {:.1}s", root.name, ui::human(root.size), root.files(), elapsed.as_secs_f64());
    if errors > 0 {
        let _ = writeln!(out, "({errors} entries unreadable)");
    }
    let _ = writeln!(out, "\nBy category:");
    let cats = root.cats();
    let mut items: Vec<_> = ALL.iter().filter(|c| cats[c.index()] > 0).collect();
    items.sort_by(|a, b| cats[b.index()].cmp(&cats[a.index()]));
    for c in items {
        let v = cats[c.index()];
        let _ = writeln!(out, "  {:<14} {:>10}  {:5.1}%", c.label(), ui::human(v), v as f64 * 100.0 / root.size.max(1) as f64);
    }
    let _ = writeln!(out, "\nLargest entries:");
    for c in root.children().iter().take(15) {
        let cats = c.cats();
        let top = ALL.iter().max_by_key(|k| cats[k.index()]).unwrap();
        let suffix = if c.is_dir() { "/" } else { "" };
        let _ = writeln!(out, "  {:>10}  {:<14} {}{}", ui::human(c.size), top.label(), c.name, suffix);
    }
    // Ignore write errors such as EPIPE when piped into `head`.
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
}
