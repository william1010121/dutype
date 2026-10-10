mod app;
mod category;
mod mcp;
mod report;
mod scan;
mod ui;

use app::App;
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use scan::Progress;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Disk usage by *kind*: see how much of a folder is video, PDFs, node_modules,
/// Python envs, build output, caches, ... Parallel and fast.
#[derive(Parser)]
#[command(version, after_help = "Config: ~/.config/dutype/config.toml (see --dump-config)")]
struct Args {
    /// Directory to analyze
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Print a text report instead of opening the TUI
    #[arg(long, conflicts_with = "json")]
    report: bool,
    /// Print a JSON report (for scripts and LLM agents)
    #[arg(long)]
    json: bool,
    /// With --report / --json: only list the largest items of this category
    /// (e.g. "node_modules", "PDF", "Python env")
    #[arg(long, value_name = "NAME")]
    category: Option<String>,
    /// Number of entries in each list of --report / --json
    #[arg(long, default_value_t = 20, value_name = "N")]
    top: usize,
    /// Descend into other filesystems / mount points
    #[arg(long)]
    cross_fs: bool,
    /// Category config file [default: ~/.config/dutype/config.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Icon set for categories [default: emoji, or `icons` in the config]
    #[arg(long, value_enum, value_name = "SET")]
    icons: Option<category::IconMode>,
    /// Print the built-in category rules (a config template) and exit
    #[arg(long)]
    dump_config: bool,
    /// Run as an MCP server on stdio, for LLM agents
    #[arg(long)]
    mcp: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dutype: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Write to stdout, ignoring EPIPE when piped into `head` and friends.
fn print(s: &str) {
    let _ = std::io::stdout().lock().write_all(s.as_bytes());
}

fn run(args: Args) -> Result<(), String> {
    if args.dump_config {
        print(category::DEFAULT_CONFIG);
        return Ok(());
    }
    category::init(args.config.as_deref(), args.icons)?;
    if args.mcp {
        return mcp::serve();
    }
    let only = match &args.category {
        Some(name) => Some(category::reg().find_by_name(name).ok_or_else(|| {
            let names: Vec<_> = category::reg().all().map(|c| c.label()).collect();
            format!("unknown category {name:?}; known: {}", names.join(", "))
        })?),
        None => None,
    };
    if only.is_some() && !(args.report || args.json) {
        return Err("--category needs --report or --json".into());
    }

    let path = std::fs::canonicalize(&args.path).map_err(|e| format!("{}: {e}", args.path.display()))?;
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let handle = {
        let (path, progress, one_fs) = (path.clone(), progress.clone(), !args.cross_fs);
        std::thread::spawn(move || scan::scan(&path, one_fs, &progress))
    };
    let finish = |handle: std::thread::JoinHandle<std::io::Result<scan::Node>>| {
        handle.join().expect("scan thread panicked").map_err(|e| format!("{}: {e}", path.display()))
    };

    if args.report || args.json {
        let root = finish(handle)?;
        let errors = progress.errors.load(Ordering::Relaxed);
        if let Some(cat) = only {
            let items = report::find(&root, &path, cat, args.top);
            if args.json {
                let doc = serde_json::json!({"path": path.display().to_string(), "category": cat.label(), "items": items});
                print(&serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?);
                print("\n");
            } else {
                print(&report::find_to_text(cat, &items));
            }
            std::mem::forget(root);
            return Ok(());
        }
        let r = report::build(&root, &path, started.elapsed(), errors, args.top);
        if args.json {
            print(&serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?);
            print("\n");
        } else {
            print(&report::to_text(&r, &root));
        }
        // Freeing millions of nodes one by one is slow; the OS reclaims it at exit.
        std::mem::forget(root);
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = (|| -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        while !handle.is_finished() {
            terminal
                .draw(|f| {
                    ui::draw_scanning(
                        f,
                        &path,
                        progress.files.load(Ordering::Relaxed),
                        progress.bytes.load(Ordering::Relaxed),
                        started.elapsed(),
                    )
                })
                .map_err(io)?;
            if event::poll(Duration::from_millis(80)).map_err(io)?
                && let Event::Key(k) = event::read().map_err(io)?
                && k.kind == KeyEventKind::Press
                && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                return Ok(());
            }
        }
        let root = finish(handle)?;
        let mut app = App::new(root, path.clone(), started.elapsed(), progress.errors.load(Ordering::Relaxed));
        while !app.quit {
            terminal.draw(|f| ui::draw(f, &mut app)).map_err(io)?;
            if let Event::Key(k) = event::read().map_err(io)?
                && k.kind == KeyEventKind::Press
            {
                app.handle_key(k);
            }
        }
        std::mem::forget(app);
        Ok(())
    })();
    ratatui::restore();
    result
}
