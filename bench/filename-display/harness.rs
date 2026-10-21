#![allow(dead_code)]
mod account;
mod app;
mod cache;
mod category;
mod display;
mod report;
mod report_baseline;
mod scan;
mod ui;
mod ui_baseline;
mod ui_probe;

use app::App;
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent},
};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static ENTRY_CALLS: AtomicU64 = AtomicU64::new(0);
struct Counter;
#[global_allocator]
static ALLOCATOR: Counter = Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, layout, size) }
    }
}
fn counts(f: impl FnOnce()) -> serde_json::Value {
    ALLOCS.store(0, Ordering::Relaxed);
    REALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    f();
    TRACK.store(false, Ordering::Relaxed);
    json!({"allocations":ALLOCS.load(Ordering::Relaxed), "reallocations":REALLOCS.load(Ordering::Relaxed), "requested_bytes":BYTES.load(Ordering::Relaxed)})
}
fn fixture(n: usize, affected: bool) -> App {
    let children: Vec<_> = (0..n)
        .map(|i| {
            let name = if affected {
                format!("a\n\\b\u{202e}-{i:05}.txt")
            } else {
                match i % 4 {
                    0 => format!("ordinary-{i:05}.txt"),
                    1 => format!("résumé-日本語-{i:05}.pdf"),
                    2 => format!("e\u{301}-👩\u{200d}💻-{i:05}.rs"),
                    _ => format!("-file-{i:05}.txt"),
                }
            };
            scan::Node {
                cat: category::reg().classify_file(&name),
                name: name.into(),
                size: 4096,
                special: false,
                hardlink: false,
                lossy: false,
                clone: false,
                dir: None,
            }
        })
        .collect();
    let mut cats = vec![0; category::reg().len()].into_boxed_slice();
    for c in &children {
        cats[c.cat.index()] += c.size;
    }
    let root = scan::Node {
        name: "root".into(),
        size: 4096 * n as u64,
        cat: category::reg().other(),
        special: false,
        hardlink: false,
        lossy: false,
        clone: false,
        dir: Some(Box::new(scan::DirInfo {
            cats,
            files: n as u64,
            errors: 0,
            children,
            free_base: 0,
            clones: None,
        })),
    };
    App::new(
        root,
        PathBuf::from("/synthetic-display-root"),
        Duration::from_millis(1234),
        0,
    )
}
fn draw(new: bool, terminal: &mut Terminal<TestBackend>, app: &mut App) {
    terminal
        .draw(|f| {
            if new {
                ui::draw(f, app)
            } else {
                ui_baseline::draw(f, app)
            }
        })
        .unwrap();
}
fn verify() {
    let mut buffers = 0;
    for (w, h) in [(240, 40), (120, 40), (80, 24), (10, 5)] {
        for n in [0, 1, 40, 10000] {
            let mut a = fixture(n, false);
            let mut b = fixture(n, false);
            let mut ta = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut tb = Terminal::new(TestBackend::new(w, h)).unwrap();
            for key in [
                None,
                Some(KeyCode::Down),
                Some(KeyCode::PageDown),
                Some(KeyCode::Char('m')),
                Some(KeyCode::Tab),
                Some(KeyCode::Enter),
                Some(KeyCode::Down),
                Some(KeyCode::Char('s')),
                Some(KeyCode::Tab),
                Some(KeyCode::Char('m')),
            ] {
                if let Some(key) = key {
                    a.handle_key(KeyEvent::from(key));
                    b.handle_key(KeyEvent::from(key));
                }
                draw(false, &mut ta, &mut a);
                draw(true, &mut tb, &mut b);
                assert_eq!(
                    ta.backend().buffer(),
                    tb.backend().buffer(),
                    "ordinary styled buffer n={n} size={w}x{h}"
                );
                assert_eq!(a.states, b.states);
                assert_eq!(a.breakdown_state, b.breakdown_state);
                buffers += 1;
            }
            ta.draw(|f| {
                ui_baseline::draw_scanning(f, &a.root_path, n as u64, a.root.size, Duration::from_millis(1234))
            })
            .unwrap();
            tb.draw(|f| ui::draw_scanning(f, &b.root_path, n as u64, b.root.size, Duration::from_millis(1234)))
                .unwrap();
            assert_eq!(ta.backend().buffer(), tb.backend().buffer());
            buffers += 1;
        }
    }
    let app = fixture(40, false);
    let meta = report::Meta {
        elapsed: Duration::from_millis(1234),
        cached_at: None,
        unreadable: 2,
        clones: false,
        clone_ids: false,
        clone_gaps: false,
        shares: false,
        reflink_fs: None,
        size_mode: report::SizeMode::Du,
        private: None,
    };
    let old_meta = report_baseline::Meta {
        elapsed: Duration::from_millis(1234),
        cached_at: None,
        unreadable: 2,
        clones: false,
        clone_ids: false,
        clone_gaps: false,
        shares: false,
        reflink_fs: None,
        size_mode: report_baseline::SizeMode::Du,
        private: None,
    };
    let new = report::build(&app.root, &app.root_path, &meta, 100);
    let old = report_baseline::build(&app.root, &app.root_path, &old_meta, 100);
    assert_eq!(
        report::to_text(&new, &app.root),
        report_baseline::to_text(&old, &app.root)
    );
    assert_eq!(serde_json::to_vec(&new).unwrap(), serde_json::to_vec(&old).unwrap());
    let cat = category::reg().classify_file("a.txt");
    let new = report::find(&app.root, &app.root_path, cat, 100, report::SizeMode::Du, true, None);
    let old = report_baseline::find(
        &app.root,
        &app.root_path,
        cat,
        100,
        report_baseline::SizeMode::Du,
        true,
        None,
    );
    assert_eq!(
        report::find_to_text(cat, &new, &meta),
        report_baseline::find_to_text(cat, &old, &old_meta)
    );
    let mut a = fixture(10000, true);
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    ENTRY_CALLS.store(0, Ordering::Relaxed);
    t.draw(|f| ui_probe::draw(f, &mut a)).unwrap();
    let calls = ENTRY_CALLS.load(Ordering::Relaxed);
    assert!(calls <= 40, "formatted offscreen names: {calls}");
    assert!(
        !t.backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.symbol().chars().any(char::is_control))
    );
    println!(
        "{}",
        json!({"mode":"verify", "styled_buffers_equal":buffers,"ordinary_reports_equal":2,
        "ordinary_json_bytes_equal":true,"rows":10000,"visible_entry_calls":calls,"terminal_backend_used":false})
    );
}
fn measure() {
    let mut results = Vec::new();
    for (label, name) in [
        ("ascii", "ordinary-file.txt".to_owned()),
        ("unicode", "résumé-日本語-👩\u{200d}💻.txt".into()),
        ("affected", "a\n\\b\u{202e}.txt".into()),
        ("heavy", "\n\\\u{202e}".repeat(64)),
    ] {
        let operation = |new| {
            if new {
                black_box(display::name(black_box(&name)));
            } else {
                black_box(black_box(&name).to_owned());
            }
        };
        let mut times = [Vec::new(), Vec::new()];
        let allocations = [counts(|| operation(false)), counts(|| operation(true))];
        for i in 0..7 {
            for new in if i % 2 == 0 { [false, true] } else { [true, false] } {
                let now = Instant::now();
                for _ in 0..50000 {
                    operation(new);
                }
                times[usize::from(new)].push(now.elapsed().as_nanos() as u64);
            }
        }
        for new in [false, true] {
            results.push(json!({"kind":"name","case":label,"candidate":new,"iterations":50000,
                "times_ns":times[usize::from(new)],"allocation_per_name":allocations[usize::from(new)]}));
        }
    }
    for affected in [false, true] {
        let mut apps = [fixture(10000, affected), fixture(10000, affected)];
        let mut terminals = [
            Terminal::new(TestBackend::new(120, 40)).unwrap(),
            Terminal::new(TestBackend::new(120, 40)).unwrap(),
        ];
        for _ in 0..5 {
            for new in [false, true] {
                let i = usize::from(new);
                draw(new, &mut terminals[i], &mut apps[i]);
            }
        }
        let allocations = [
            counts(|| draw(false, &mut terminals[0], &mut apps[0])),
            counts(|| draw(true, &mut terminals[1], &mut apps[1])),
        ];
        let mut times = [Vec::new(), Vec::new()];
        for batch in 0..7 {
            for new in if batch % 2 == 0 { [false, true] } else { [true, false] } {
                let i = usize::from(new);
                let now = Instant::now();
                for _ in 0..20 {
                    draw(new, &mut terminals[i], &mut apps[i]);
                }
                times[i].push(now.elapsed().as_nanos() as u64);
            }
        }
        for new in [false, true] {
            let i = usize::from(new);
            results.push(json!({"kind":"frame","case":if affected {"affected"} else {"ordinary"},"candidate":new,
                "frames":20,"times_ns":times[i],"allocation_per_frame":allocations[i],"rows":10000,"width":120,"height":40}));
        }
    }
    println!(
        "{}",
        json!({"mode":"measure","results":results,"terminal_backend_used":false})
    );
}
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("measure") => measure(),
        _ => verify(),
    }
}
