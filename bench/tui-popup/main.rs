#![allow(dead_code)]
// Standalone driver: all modules are actual, unmodified production source copies.
mod account;
mod app;
mod cache;
mod category;
mod report;
mod scan;
mod ui;

use app::{App, BreakdownItem, Focus, Key};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use scan::{DirInfo, Node};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(feature = "bench_metrics")]
mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
    pub struct Counter;
    static ENABLED: AtomicBool = AtomicBool::new(false);
    static ALLOCS: AtomicU64 = AtomicU64::new(0);
    static ZEROED: AtomicU64 = AtomicU64::new(0);
    static REALLOCS: AtomicU64 = AtomicU64::new(0);
    static DEALLOCS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
    static FREE_BYTES: AtomicU64 = AtomicU64::new(0);
    static LIVE: AtomicI64 = AtomicI64::new(0);
    static PEAK: AtomicI64 = AtomicI64::new(0);
    fn add(bytes: usize) {
        ALLOC_BYTES.fetch_add(bytes as u64, Relaxed);
        let live = LIVE.fetch_add(bytes as i64, Relaxed) + bytes as i64;
        PEAK.fetch_max(live, Relaxed);
    }
    fn remove(bytes: usize) {
        FREE_BYTES.fetch_add(bytes as u64, Relaxed);
        LIVE.fetch_sub(bytes as i64, Relaxed);
    }
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() && ENABLED.load(Relaxed) {
                ALLOCS.fetch_add(1, Relaxed);
                add(layout.size());
            }
            ptr
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() && ENABLED.load(Relaxed) {
                ALLOCS.fetch_add(1, Relaxed);
                ZEROED.fetch_add(1, Relaxed);
                add(layout.size());
            }
            ptr
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            if ENABLED.load(Relaxed) {
                DEALLOCS.fetch_add(1, Relaxed);
                remove(layout.size());
            }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let next = unsafe { System.realloc(ptr, layout, size) };
            if !next.is_null() && ENABLED.load(Relaxed) {
                REALLOCS.fetch_add(1, Relaxed);
                remove(layout.size());
                add(size);
            }
            next
        }
    }
    #[derive(serde::Serialize)]
    pub struct Snapshot {
        pub alloc_calls: u64,
        pub zeroed_alloc_calls: u64,
        pub realloc_calls: u64,
        pub dealloc_calls: u64,
        pub allocated_bytes: u64,
        pub deallocated_bytes: u64,
        pub peak_live_bytes: i64,
        pub retained_bytes: i64,
    }
    pub fn begin() {
        assert!(!ENABLED.load(Relaxed));
        for value in [
            &ALLOCS,
            &ZEROED,
            &REALLOCS,
            &DEALLOCS,
            &ALLOC_BYTES,
            &FREE_BYTES,
        ] {
            value.store(0, Relaxed);
        }
        LIVE.store(0, Relaxed);
        PEAK.store(0, Relaxed);
        ENABLED.store(true, Relaxed);
    }
    pub fn end() -> Snapshot {
        ENABLED.store(false, Relaxed);
        Snapshot {
            alloc_calls: ALLOCS.load(Relaxed),
            zeroed_alloc_calls: ZEROED.load(Relaxed),
            realloc_calls: REALLOCS.load(Relaxed),
            dealloc_calls: DEALLOCS.load(Relaxed),
            allocated_bytes: ALLOC_BYTES.load(Relaxed),
            deallocated_bytes: FREE_BYTES.load(Relaxed),
            peak_live_bytes: PEAK.load(Relaxed),
            retained_bytes: LIVE.load(Relaxed),
        }
    }
}
#[cfg(feature = "bench_metrics")]
#[global_allocator]
static ALLOCATOR: allocations::Counter = allocations::Counter;

const LIMIT: usize = 500;
const ROOT_PATH: &str = "/synthetic-not-on-disk";
const VARIANT: &str = include_str!("bench_variant.txt");
const EXPECTATION: &str = include_str!("bench_expectation.txt");
const APP_SHA: &str = include_str!("bench_app_sha256.txt");

fn tree(count: usize, pattern: &str) -> Node {
    let cat = category::reg().classify_file("a.pdf");
    let children: Vec<_> = (0..count)
        .map(|i| Node {
            name: format!("f{i:08}.pdf").into(),
            size: if pattern == "descending" {
                (count - i) as u64 * 4096
            } else {
                4096
            },
            cat,
            special: false,
            hardlink: false,
            lossy: false,
            clone: false,
            dir: None,
        })
        .collect();
    let total = children.iter().map(|n| n.size).sum();
    let mut cats = vec![0; category::reg().len()].into_boxed_slice();
    cats[cat.index()] = total;
    Node {
        name: "root".into(),
        size: total,
        cat: category::reg().other(),
        special: false,
        hardlink: false,
        lossy: false,
        clone: false,
        dir: Some(Box::new(DirInfo {
            cats,
            files: count as u64,
            errors: 0,
            children,
            free_base: total,
            clones: None,
        })),
    }
}

fn application(count: usize, pattern: &str, pane: &str) -> App {
    let mut app = App::new(
        tree(count, pattern),
        PathBuf::from(ROOT_PATH),
        Duration::ZERO,
        0,
    );
    let cat = category::reg().classify_file("a.pdf");
    let key = if pane == "category" {
        Key::Cat(cat)
    } else {
        Key::Ext("pdf".into())
    };
    app.focus = Focus::Breakdown;
    app.breakdown = vec![BreakdownItem {
        key,
        label: "fixture".into(),
        cat,
        size: app.root.size,
        count: Some(count as u64),
    }];
    app.breakdown_state.select(Some(0));
    app
}

fn open(app: &mut App) {
    assert!(app.popup.is_none());
    app.handle_key(KeyEvent::from(KeyCode::Enter));
    black_box(&app.popup);
}

fn digest(app: &App, count: usize) -> u64 {
    let popup = app.popup.as_ref().expect("popup exists");
    assert_eq!(popup.items.len(), count.min(LIMIT));
    popup
        .items
        .iter()
        .fold(0xcbf29ce484222325u64, |mut hash, item| {
            for byte in item
                .path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .copied()
                .chain(item.size.to_le_bytes())
            {
                hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
            }
            hash
        })
}

fn verify(app: &mut App, count: usize, pattern: &str) -> serde_json::Value {
    open(app);
    let checksum = digest(app, count);
    let popup = app.popup.as_ref().unwrap();
    let cat = category::reg().classify_file("a.pdf");
    let oracle = report::find(
        &app.root,
        &app.root_path,
        cat,
        LIMIT,
        report::SizeMode::Du,
        false,
        None,
    );
    let parity = popup.items.len() == oracle.items.len()
        && popup
            .items
            .iter()
            .zip(&oracle.items)
            .all(|(item, expected)| {
                item.size == expected.size_bytes
                    && app.root_path.join(&item.path).display().to_string() == expected.path
            });
    let expected_parity = EXPECTATION == "correct" || pattern != "ties" || count <= LIMIT;
    assert_eq!(
        parity, expected_parity,
        "unexpected report parity for selected source expectation"
    );
    let indices: Vec<_> = match pattern {
        "ties" if EXPECTATION == "correct" => (count.saturating_sub(LIMIT)..count).rev().collect(),
        "ties" => (0..count.min(LIMIT)).rev().collect(),
        "descending" => (0..count.min(LIMIT)).collect(),
        _ => unreachable!(),
    };
    for (item, index) in popup.items.iter().zip(indices) {
        assert_eq!(item.path, PathBuf::from(format!("f{index:08}.pdf")));
        assert_eq!(item.fs_path, Some(app.root_path.join(&item.path)));
    }
    serde_json::json!({
        "report_parity": parity,
        "expected_parity": expected_parity,
        "filesystem_paths_verified": true,
        "checksum": checksum,
        "first_path": popup.items.first().unwrap().path,
        "last_path": popup.items.last().unwrap().path,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "identity" {
        println!(
            "{}",
            serde_json::json!({
                "variant": VARIANT,
                "expectation": EXPECTATION,
                "instrumented": cfg!(feature = "bench_metrics"),
                "app_rs_sha256": APP_SHA,
            })
        );
        return;
    }
    assert!(
        (5..=7).contains(&args.len()),
        "usage: binary time|metrics|rss|verify COUNT ties|descending category|extension [WARMUPS=2] [REPEATS=7]"
    );
    let mode = args[1].as_str();
    let count: usize = args[2].parse().unwrap();
    let pattern = args[3].as_str();
    let pane = args[4].as_str();
    let warmups: usize = args.get(5).map_or(2, |s| s.parse().unwrap());
    let repeats: usize = args.get(6).map_or(7, |s| s.parse().unwrap());
    assert!((1..=100_000).contains(&count));
    assert!(matches!(pattern, "ties" | "descending"));
    assert!(matches!(pane, "category" | "extension"));
    assert!(warmups <= 2 && (1..=7).contains(&repeats));
    assert!(matches!(mode, "time" | "metrics" | "rss" | "verify"));
    if mode == "metrics" {
        assert!(cfg!(feature = "bench_metrics"), "use metrics binary");
    }
    if matches!(mode, "time" | "rss") {
        assert!(
            !cfg!(feature = "bench_metrics"),
            "use ordinary-allocator binary"
        );
    }
    // One tree, registry and all App/breakdown setup precede timing/counters.
    let mut app = application(count, pattern, pane);
    let result = match mode {
        "time" => {
            for _ in 0..warmups {
                open(&mut app);
                black_box(digest(&app, count));
                app.popup = None;
            }
            let mut samples_ns = Vec::with_capacity(repeats);
            let mut checksum = 0;
            for _ in 0..repeats {
                let start = Instant::now();
                open(&mut app);
                let elapsed = start.elapsed();
                samples_ns.push(elapsed.as_nanos() as u64);
                checksum = digest(&app, count);
                app.popup = None; // Destruction is outside the opening interval.
            }
            serde_json::json!({"warmups":warmups,"repeats":repeats,"samples_ns":samples_ns,"checksum":checksum})
        }
        "metrics" => {
            #[cfg(feature = "bench_metrics")]
            {
                allocations::begin();
                open(&mut app);
                let snapshot = allocations::end();
                assert!(snapshot.retained_bytes >= 0);
                serde_json::json!({"allocator":snapshot,"checksum":digest(&app,count)})
            }
            #[cfg(not(feature = "bench_metrics"))]
            {
                unreachable!()
            }
        }
        "rss" => {
            open(&mut app);
            serde_json::json!({"checksum":digest(&app,count),"includes":"full fresh process, registry, one tree, App, one popup, output and teardown"})
        }
        "verify" => verify(&mut app, count, pattern),
        _ => unreachable!(),
    };
    println!(
        "{}",
        serde_json::json!({
            "variant":VARIANT,"mode":mode,"count":count,"pattern":pattern,"pane":pane,
            "popup_limit":LIMIT,"instrumented":cfg!(feature="bench_metrics"),"result":result,
        })
    );
}
