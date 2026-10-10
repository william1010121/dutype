mod app;
mod cache;
mod category;
mod mcp;
mod report;
mod scan;
mod ui;

use app::App;
use cache::Snapshot;
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
    /// Show the last key pressed in the TUI (for demos and screencasts)
    #[arg(long, hide = true)]
    show_keys: bool,
    /// Run as an MCP server on stdio, for LLM agents
    #[arg(long)]
    mcp: bool,
    /// Reuse the last scan of this path if there is one, and save new scans
    /// (in ~/.cache/dutype, or $DUTYPE_CACHE_DIR)
    #[arg(long)]
    cache: bool,
    /// Ignore a cached scan: scan again and update the cache (implies --cache)
    #[arg(long)]
    refresh: bool,
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
    let use_cache = args.cache || args.refresh;
    if args.mcp {
        return mcp::serve(use_cache);
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
    let one_fs = !args.cross_fs;
    let cached = || {
        (use_cache && !args.refresh)
            .then(|| cache::load(&path, one_fs))
            .flatten()
    };

    if args.report || args.json {
        let snap = match cached() {
            Some(s) => s,
            None => {
                let progress = Progress::default();
                let started = Instant::now();
                let root = scan::scan(&path, one_fs, &progress).map_err(|e| format!("{}: {e}", path.display()))?;
                let snap = Snapshot {
                    root,
                    cached_at: None,
                    scan_time: started.elapsed(),
                    errors: progress.errors.load(Ordering::Relaxed),
                };
                if use_cache && let Err(e) = cache::save(&path, one_fs, &snap) {
                    eprintln!("dutype: cannot write cache: {e}");
                }
                snap
            }
        };
        let root = &snap.root;
        if let Some(cat) = only {
            let items = report::find(root, &path, cat, args.top);
            if args.json {
                let doc = serde_json::json!({
                    "path": path.display().to_string(),
                    "category": cat.label(),
                    "cached_at": report::unix_secs(snap.cached_at),
                    "items": items,
                });
                print(&serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?);
                print("\n");
            } else {
                print(&report::find_to_text(cat, &items));
            }
        } else {
            let r = report::build(root, &path, snap.scan_time, snap.cached_at, snap.errors, args.top);
            if args.json {
                print(&serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?);
                print("\n");
            } else {
                print(&report::to_text(&r, root));
            }
        }
        // Freeing millions of nodes one by one is slow; the OS reclaims it at exit.
        std::mem::forget(snap);
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = (|| -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        let mut snap = cached();
        let mut location = Vec::new();
        loop {
            let snap = match snap.take() {
                Some(s) => s,
                None => {
                    let progress = Arc::new(Progress::default());
                    let started = Instant::now();
                    let handle = {
                        let (path, progress) = (path.clone(), progress.clone());
                        std::thread::spawn(move || scan::scan(&path, one_fs, &progress))
                    };
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
                    let root = handle
                        .join()
                        .expect("scan thread panicked")
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    Snapshot {
                        root,
                        cached_at: None,
                        scan_time: started.elapsed(),
                        errors: progress.errors.load(Ordering::Relaxed),
                    }
                }
            };
            let save_error = match use_cache && snap.cached_at.is_none() {
                true => cache::save(&path, one_fs, &snap).err(),
                false => None,
            };
            let mut app = App::new(snap.root, path.clone(), snap.scan_time, snap.errors);
            app.cached_at = snap.cached_at;
            app.show_keys = args.show_keys;
            app.go_to(&location);
            app.status = save_error.map(|e| format!("cannot write cache: {e}"));
            while !app.quit {
                terminal.draw(|f| ui::draw(f, &mut app)).map_err(io)?;
                if let Event::Key(k) = event::read().map_err(io)?
                    && k.kind == KeyEventKind::Press
                {
                    app.handle_key(k);
                }
            }
            if !app.rescan {
                std::mem::forget(app);
                return Ok(());
            }
            location = app.location();
            // Free the old tree off the UI thread; it can take a while.
            std::thread::spawn(move || drop(app));
        }
    })();
    ratatui::restore();
    result
}
