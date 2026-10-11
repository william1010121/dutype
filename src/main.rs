mod account;
mod app;
mod cache;
mod category;
mod mcp;
mod report;
mod scan;
mod ui;

use app::{App, Rescan};
use cache::Snapshot;
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use scan::Progress;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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
    /// Size to show and sort by: `du` (allocated, like `du -skx`) or
    /// `dedup` (each APFS clone family counted once). `c` toggles it in the TUI
    ///
    /// Only full clones are deduplicated (a clone modified since counts in
    /// full); APFS snapshots are not considered; families are relative to the
    /// scanned tree. --report / --json also give a lower bound of what
    /// deleting each item frees (freeable_min_bytes).
    #[arg(long, value_enum, default_value_t = report::SizeMode::Du, value_name = "MODE")]
    size_mode: report::SizeMode,
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
        return mcp::serve(mcp::CacheOpts {
            use_cache,
            always_refresh: args.refresh,
        });
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
    // After validating the target (so an invalid path is never created as a
    // side effect) and before any scan (see cache::prepare).
    if use_cache && let Err(e) = cache::prepare() {
        eprintln!("dutype: cannot create cache directory: {e}");
    }
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
                let root_dev = scan::device_of(&path).ok();
                let root = scan::scan(&path, one_fs, &progress).map_err(|e| format!("{}: {e}", path.display()))?;
                let snap = Snapshot {
                    root,
                    cached_at: None,
                    scan_time: started.elapsed(),
                    errors: progress.errors.load(Ordering::Relaxed),
                    root_dev,
                    clone_ids: progress.clone_ids.load(Ordering::Relaxed),
                };
                if use_cache && let Err(e) = cache::save(&path, one_fs, &snap) {
                    eprintln!("dutype: cannot write cache: {e}");
                }
                snap
            }
        };
        let root = &snap.root;
        if let Some(cat) = only {
            let items = report::find(root, &path, cat, args.top, args.size_mode);
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
            let r = report::build(
                root,
                &path,
                &report::Meta {
                    elapsed: snap.scan_time,
                    cached_at: snap.cached_at,
                    unreadable: snap.errors,
                    clone_ids: snap.clone_ids,
                    size_mode: args.size_mode,
                },
                args.top,
            );
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
        // Status to show after a full rescan (e.g. why `r` became `R`).
        let mut carried_status: Option<String> = None;
        // `c` survives a full rescan.
        let mut size_mode = args.size_mode;
        loop {
            // The root's device when a fresh tree is scanned (measured before
            // the scan); a cached tree carries its own. `r` compares against it.
            let root_dev = scan::device_of(&path).ok();
            let snap = match snap.take() {
                Some(s) => s,
                None => {
                    let p = path.clone();
                    let scanned = scan_with_progress(&mut terminal, &path, move |pr| scan::scan(&p, one_fs, pr))?;
                    let Some((root, scan_time, errors, clone_ids)) = scanned else {
                        return Ok(());
                    };
                    Snapshot {
                        root,
                        cached_at: None,
                        scan_time,
                        errors,
                        root_dev,
                        clone_ids,
                    }
                }
            };
            let save_error = match use_cache && snap.cached_at.is_none() {
                true => cache::save(&path, one_fs, &snap).err(),
                false => None,
            };
            let snap_root_dev = snap.root_dev;
            let clone_ids = snap.clone_ids;
            let mut app = App::new(snap.root, path.clone(), snap.scan_time, snap.errors);
            app.cached_at = snap.cached_at;
            app.root_dev = snap_root_dev;
            app.clone_ids = clone_ids;
            app.set_size_mode(size_mode);
            app.show_keys = args.show_keys;
            app.go_to(&location);
            app.status = save_error
                .map(|e| format!("cannot write cache: {e}"))
                .or(carried_status.take());
            while !app.quit {
                terminal.draw(|f| ui::draw(f, &mut app)).map_err(io)?;
                if let Event::Key(k) = event::read().map_err(io)?
                    && k.kind == KeyEventKind::Press
                {
                    app.handle_key(k);
                }
                if app.rescan == Some(Rescan::Current) {
                    app.rescan = None;
                    rescan_current(&mut terminal, &mut app, one_fs, use_cache)?;
                }
            }
            if app.rescan != Some(Rescan::All) {
                std::mem::forget(app);
                return Ok(());
            }
            location = app.location();
            carried_status = app.status.take();
            size_mode = app.size_mode;
            // Free the old tree off the UI thread; it can take a while.
            std::thread::spawn(move || drop(app));
        }
    })();
    ratatui::restore();
    result
}

/// Run `scan` on a thread while drawing the progress screen. `Ok(None)` when
/// the user aborts with q / Esc.
fn scan_with_progress<T: Send + 'static>(
    terminal: &mut ratatui::DefaultTerminal,
    path: &Path,
    scan: impl FnOnce(&Progress) -> std::io::Result<T> + Send + 'static,
) -> Result<Option<(T, Duration, u64, bool)>, String> {
    let io = |e: std::io::Error| e.to_string();
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let handle = {
        let progress = progress.clone();
        std::thread::spawn(move || scan(&progress))
    };
    while !handle.is_finished() {
        terminal
            .draw(|f| {
                ui::draw_scanning(
                    f,
                    path,
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
            // Stop the worker (it checks the flag per directory) and wait for
            // it, so a cancelled scan never keeps walking in the background.
            progress.cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
            return Ok(None);
        }
    }
    let root = handle
        .join()
        .expect("scan thread panicked")
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some((
        root,
        started.elapsed(),
        progress.errors.load(Ordering::Relaxed),
        progress.clone_ids.load(Ordering::Relaxed),
    )))
}

/// `r`: rescan the folder the user is in, splice it into the tree and update
/// the `--cache` snapshot. Failures and aborts keep the old tree.
fn rescan_current(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    one_fs: bool,
    use_cache: bool,
) -> Result<(), String> {
    // Hard links (in the old subtree or the new one) may be counted elsewhere
    // in the tree; only a full rescan deduplicates them correctly.
    let full_rescan = |app: &mut App| {
        app.rescan = Some(Rescan::All);
        app.quit = true;
        app.status = Some("hard links here: rescanning everything".into());
    };
    // A non-UTF-8 folder name on the way may collide with a sibling's name.
    // Every folder on the way must still be a real directory (not a symlink).
    let target = match app.fs_path_checked() {
        Some(t) if !app.path_has_lossy_name() => t,
        _ => {
            app.rescan = Some(Rescan::All);
            app.quit = true;
            app.status = Some("path changed or has a non-UTF-8 name: rescanning everything".into());
            return Ok(());
        }
    };
    if app.current().hardlink {
        full_rescan(app);
        return Ok(());
    }
    // Replaced by a symlink or a file, or something got mounted on it (or on
    // the root) since the scan: a full rescan treats it like the original.
    let root_dev = app.root_dev;
    let same_dev = |p: &Path| root_dev.is_some() && scan::device_of(p).ok() == root_dev;
    let moved_fs = one_fs && !(same_dev(&target) && same_dev(&app.root_path));
    if !scan::is_real_dir(&target) || moved_fs {
        app.rescan = Some(Rescan::All);
        app.quit = true;
        app.status = Some(format!("{} changed: rescanning everything", ui::display_path(&target)));
        return Ok(());
    }
    let inherited = app.inherited_category();
    let t = target.clone();
    let scanned = scan_with_progress(terminal, &target, move |pr| {
        scan::scan_subtree(&t, one_fs, pr, inherited, root_dev)
    });
    if let Ok(Some((sub, ..))) = &scanned
        && sub.hardlink
    {
        full_rescan(app);
        return Ok(());
    }
    app.status = Some(match scanned {
        Ok(Some((sub, took, ..))) => {
            // replace_current swaps in the subtree's own error count.
            app.replace_current(sub);
            app.cached_at = None;
            let saved = use_cache.then(|| save_tree(app, one_fs)).flatten();
            match saved {
                Some(e) => format!("cannot write cache: {e}"),
                None => format!("rescanned {} in {:.2}s", ui::display_path(&target), took.as_secs_f64()),
            }
        }
        Ok(None) => "rescan cancelled".into(),
        Err(e) => format!("rescan failed: {e}"),
    });
    Ok(())
}

/// Save the app's (partly rescanned) tree to the cache; returns the error, if any.
fn save_tree(app: &mut App, one_fs: bool) -> Option<std::io::Error> {
    let placeholder = scan::Node {
        name: "".into(),
        size: 0,
        cat: category::reg().other(),
        special: false,
        hardlink: false,
        lossy: false,
        clone: false,
        dir: None,
    };
    let root_path = app.root_path.clone();
    let (scan_time, errors, root_dev, clone_ids) = (app.scan_time, app.errors, app.root_dev, app.clone_ids);
    // A cache holds the tree in du order, whatever the TUI shows.
    app.in_du_order(|root| {
        let snap = Snapshot {
            root: std::mem::replace(root, placeholder),
            cached_at: None,
            scan_time,
            errors,
            root_dev,
            clone_ids,
        };
        let result = cache::save(&root_path, one_fs, &snap).err();
        *root = snap.root;
        result
    })
}
