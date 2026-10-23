#![allow(dead_code)]
mod account;
mod app;
mod cache;
mod category;
mod display;
mod report;
mod scan;
mod ui;
#[path = "../bench/shallow-redraw/ui_baseline.rs"] mod ui_baseline;
#[path = "../bench/shallow-redraw/ui_probe.rs"] mod ui_probe;

use app::App;
use ratatui::{backend::TestBackend, crossterm::event::{KeyCode, KeyEvent, KeyModifiers}, layout::Rect, Terminal};
use std::{alloc::{GlobalAlloc, Layout, System}, path::PathBuf, sync::atomic::{AtomicBool, AtomicU64, AtomicI64, Ordering}, time::{Duration, Instant}};

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);
static LIVE_DELTA: AtomicI64 = AtomicI64::new(0);
static PEAK_DELTA: AtomicI64 = AtomicI64::new(0);
fn track_live(delta:i64) { let now=LIVE_DELTA.fetch_add(delta,Ordering::Relaxed)+delta; PEAK_DELTA.fetch_max(now,Ordering::Relaxed); }
pub static ENTRY_CALLS: AtomicU64 = AtomicU64::new(0);
pub static APPORTION_CALLS: AtomicU64 = AtomicU64::new(0);
struct Counter;
#[global_allocator] static ALLOCATOR: Counter = Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) { ALLOCS.fetch_add(1, Ordering::Relaxed); BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed); track_live(layout.size() as i64); }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) { ALLOCS.fetch_add(1, Ordering::Relaxed); BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed); track_live(layout.size() as i64); }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) { DEALLOCS.fetch_add(1, Ordering::Relaxed); track_live(-(layout.size() as i64)); }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) { REALLOCS.fetch_add(1, Ordering::Relaxed); BYTES.fetch_add(new_size as u64, Ordering::Relaxed); track_live(new_size as i64-layout.size() as i64); }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
fn fixture_count(count: usize, zero: bool, labels: bool) -> App {
    let exts = ["rs", "pdf", "mp4", "json", "png", "txt", "zip", "py"];
    let mut children: Vec<scan::Node> = (0..count).map(|i| {
        let name = if labels { match i % 6 {
            0 => format!("日本語-🦀-{i}.rs"),
            1 => format!("multi-line-{i}.txt"),
            2 => format!("combining-e\u{301}-{i}.pdf"),
            3 => format!("{}-{i}.mp4", "long".repeat(40)),
            4 => format!("tab-name-{i}.zip"),
            _ => format!("file-{i:05}.{}", exts[i % exts.len()]),
        }} else { format!("file-{i:05}.{}", exts[i % exts.len()]) };
        scan::Node { cat: category::reg().classify_file(&name), name: name.into(), size: if zero { 0 } else { 4096 * (1 + ((i * 7919) % 256)) as u64 }, special:false, hardlink:false, lossy:false, clone:false, dir:None }
    }).collect();
    children.sort_by(|a,b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    let mut cats = vec![0; category::reg().len()].into_boxed_slice();
    for child in &children { cats[child.cat.index()] += child.size; }
    let root = scan::Node { name: "shallow-10000".into(), size: children.iter().map(|c| c.size).sum(), cat: category::reg().other(), special:false, hardlink:false, lossy:false, clone:false, dir:Some(Box::new(scan::DirInfo { cats, files:count as u64, errors:0, children, free_base:0, clones:None })) };
    App::new(root, PathBuf::from("/synthetic-shallow-10000"), Duration::ZERO, 0)
}
fn fixture() -> App { fixture_count(10_000,false,false) }
fn cpu_ns() -> u64 {
    let mut t = libc::timespec { tv_sec:0, tv_nsec:0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) }, 0);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn reset_counts() { LIVE_DELTA.store(0,Ordering::Relaxed); PEAK_DELTA.store(0,Ordering::Relaxed); for c in [&ALLOCS,&BYTES,&REALLOCS,&DEALLOCS,&ENTRY_CALLS,&APPORTION_CALLS] { c.store(0,Ordering::Relaxed); } }
fn prepare(case:&str, i:usize, terminal:&mut Terminal<TestBackend>, app:&mut App) {
    match case {
        "navigation" => {
            let keys=[KeyCode::Down,KeyCode::PageDown,KeyCode::End,KeyCode::Up,KeyCode::Home,KeyCode::PageUp];
            app.handle_key(KeyEvent::new(keys[i%keys.len()],KeyModifiers::NONE));
        },
        "resize" => { let (w,h)=if i%2==0 {(120,40)} else {(100,30)}; terminal.backend_mut().resize(w,h); terminal.resize(Rect::new(0,0,w,h)).unwrap(); },
        "unchanged"=>{},
        _=>panic!("unknown case")
    }
}
fn baseline_main() {
    println!("fixture files=10000 depth=1 category_count={} no_filesystem_reads=true", category::reg().len());
    for case in ["unchanged","navigation","resize"] {
        let mut app=fixture();
        let mut terminal=Terminal::new(TestBackend::new(120,40)).unwrap();
        for i in 0..8 { prepare(case,i,&mut terminal,&mut app); terminal.draw(|f| ui::draw(f,&mut app)).unwrap(); }
        for batch in 0..5 {
            let start=Instant::now(); let cpu=cpu_ns();
            for i in 0..40 { prepare(case,i,&mut terminal,&mut app); terminal.draw(|f| ui::draw(f,&mut app)).unwrap(); }
            println!("timing case={case} batch={batch} frames=40 wall_ms={:.3} cpu_ms={:.3}",start.elapsed().as_secs_f64()*1000.0,(cpu_ns()-cpu) as f64/1e6);
        }
        reset_counts(); COUNT_ALLOCATIONS.store(true,Ordering::Relaxed);
        for i in 0..4 { prepare(case,i,&mut terminal,&mut app); terminal.draw(|f| ui::draw(f,&mut app)).unwrap(); }
        COUNT_ALLOCATIONS.store(false,Ordering::Relaxed);
        println!("allocation case={case} frames=4 alloc={} realloc={} dealloc={} requested_bytes={}",ALLOCS.load(Ordering::Relaxed),REALLOCS.load(Ordering::Relaxed),DEALLOCS.load(Ordering::Relaxed),BYTES.load(Ordering::Relaxed));
    }
    // Identical production and counter-only copies must preserve full styled buffers and table state.
    let mut a=fixture(); let mut b=fixture();
    let mut ta=Terminal::new(TestBackend::new(120,40)).unwrap();
    let mut tb=Terminal::new(TestBackend::new(120,40)).unwrap();
    reset_counts();
    for case in ["unchanged","navigation","resize"] {
        for i in 0..12 {
            prepare(case,i,&mut ta,&mut a); prepare(case,i,&mut tb,&mut b);
            ta.draw(|f|ui::draw(f,&mut a)).unwrap(); tb.draw(|f|ui_probe::draw(f,&mut b)).unwrap();
            assert_eq!(ta.backend().buffer(),tb.backend().buffer());
            assert_eq!(a.states,b.states);
        }
    }
    println!("probe frames=36 entry_name_calls={} apportion_calls={} styled_buffers_equal=true table_states_equal=true",ENTRY_CALLS.load(Ordering::Relaxed),APPORTION_CALLS.load(Ordering::Relaxed));
}

fn render_pair(a:&mut App,b:&mut App,ta:&mut Terminal<TestBackend>,tb:&mut Terminal<TestBackend>,label:&str) {
    ta.draw(|f|ui_baseline::draw(f,a)).unwrap();
    tb.draw(|f|ui::draw(f,b)).unwrap();
    assert_eq!(ta.backend().buffer(),tb.backend().buffer(), "buffer mismatch: {label}");
    assert_eq!(a.states,b.states,"entry state mismatch: {label}");
    assert_eq!(a.breakdown_state,b.breakdown_state,"breakdown state mismatch: {label}");
    assert_eq!(a.stack,b.stack,"stack mismatch: {label}");
}
fn verify() {
    use ratatui::widgets::TableState;
    let mut frames=0;
    for count in [0usize,1,2,24,31,32,33,100,10_000] {
        let mut a=fixture_count(count,false,true); let mut b=fixture_count(count,false,true);
        for (w,h) in [(0,0),(1,1),(2,40),(4,7),(10,8),(120,9),(10,40),(120,40),(100,30),(240,80)] {
            let mut ta=Terminal::new(TestBackend::new(w,h)).unwrap();
            let mut tb=Terminal::new(TestBackend::new(w,h)).unwrap();
            let states=[(None,0),(None,count.saturating_sub(1)),(None,count+17),(Some(0),count+99),(Some(count.saturating_sub(1)),0),(Some(count+99),count+99),(Some(usize::MAX),usize::MAX)];
            for (selected,offset) in states { for column in [None,Some(0),Some(3),Some(usize::MAX)] {
                let state=TableState::default().with_selected(selected).with_offset(offset).with_selected_column(column);
                a.states=vec![state.clone()]; b.states=vec![state];
                let label=format!("count={count} area={w}x{h} selected={selected:?} offset={offset} col={column:?}");
                render_pair(&mut a,&mut b,&mut ta,&mut tb,&label); frames+=1;
            }}
        }
        a.states=vec![TableState::default().with_selected((count>0).then_some(0))];
        b.states=a.states.clone();
        let mut ta=Terminal::new(TestBackend::new(120,40)).unwrap();
        let mut tb=Terminal::new(TestBackend::new(120,40)).unwrap();
        for i in 0..36 {
            prepare("navigation",i,&mut ta,&mut a); prepare("navigation",i,&mut tb,&mut b);
            if i%3==0 { prepare("resize",i,&mut ta,&mut a); prepare("resize",i,&mut tb,&mut b); }
            render_pair(&mut a,&mut b,&mut ta,&mut tb,&format!("navigation count={count} step={i}")); frames+=1;
        }
    }
    for count in [1,33,10_000] {
        let mut a=fixture_count(count,true,true); let mut b=fixture_count(count,true,true);
        let mut ta=Terminal::new(TestBackend::new(120,40)).unwrap(); let mut tb=Terminal::new(TestBackend::new(120,40)).unwrap();
        for i in 0..12 { prepare("navigation",i,&mut ta,&mut a);prepare("navigation",i,&mut tb,&mut b);render_pair(&mut a,&mut b,&mut ta,&mut tb,"zero sizes");frames+=1; }
    }
    println!("differential frames={frames} full_styled_buffers_equal=true global_table_states_equal=true geometry_empty_tiny_normal=true stale_offsets_selection_columns=true unicode_long_zero_sizes=true");
}
fn benchmark(counts:&[usize],frames:usize) {
    for &count in counts { for case in ["unchanged","navigation","resize","tiny"] {
        let (width,height)=if case=="tiny" {(4,7)} else {(120,40)};
        let preparation=if case=="tiny" {"unchanged"} else {case};
        let mut a=fixture_count(count,false,false); let mut b=fixture_count(count,false,false);
        let mut ta=Terminal::new(TestBackend::new(width,height)).unwrap();let mut tb=Terminal::new(TestBackend::new(width,height)).unwrap();
        for i in 0..8 { prepare(preparation,i,&mut ta,&mut a);prepare(preparation,i,&mut tb,&mut b);ta.draw(|f|ui_baseline::draw(f,&mut a)).unwrap();tb.draw(|f|ui::draw(f,&mut b)).unwrap(); }
        for batch in 0..5 { for variant in if batch%2==0 {["baseline","candidate"]} else {["candidate","baseline"]} {
            let (terminal,app)=if variant=="baseline" {(&mut ta,&mut a)} else {(&mut tb,&mut b)};
            let start=Instant::now();let cpu=cpu_ns();
            for i in 0..frames { prepare(preparation,i,terminal,app);terminal.draw(|f|if variant=="baseline" {ui_baseline::draw(f,app)} else {ui::draw(f,app)}).unwrap(); }
            println!("timing files={count} case={case} variant={variant} batch={batch} frames={frames} wall_ms={:.3} cpu_ms={:.3}",start.elapsed().as_secs_f64()*1000.0,(cpu_ns()-cpu) as f64/1e6);
        }}
        for variant in ["baseline","candidate"] {
            let (terminal,app)=if variant=="baseline" {(&mut ta,&mut a)} else {(&mut tb,&mut b)};
            reset_counts();COUNT_ALLOCATIONS.store(true,Ordering::Relaxed);
            for i in 0..4 { prepare(preparation,i,terminal,app);terminal.draw(|f|if variant=="baseline" {ui_baseline::draw(f,app)} else {ui::draw(f,app)}).unwrap(); }
            COUNT_ALLOCATIONS.store(false,Ordering::Relaxed);
            println!("allocation files={count} case={case} variant={variant} frames=4 alloc={} realloc={} dealloc={} requested_bytes={} peak_live_delta_bytes={} end_live_delta_bytes={}",ALLOCS.load(Ordering::Relaxed),REALLOCS.load(Ordering::Relaxed),DEALLOCS.load(Ordering::Relaxed),BYTES.load(Ordering::Relaxed),PEAK_DELTA.load(Ordering::Relaxed),LIVE_DELTA.load(Ordering::Relaxed));
        }
    }}
    let mut app=fixture(); let mut terminal=Terminal::new(TestBackend::new(120,40)).unwrap();
    reset_counts();terminal.draw(|f|ui_probe::draw(f,&mut app)).unwrap();
    println!("candidate_probe frames=1 entry_name_calls={} apportion_calls={}",ENTRY_CALLS.load(Ordering::Relaxed),APPORTION_CALLS.load(Ordering::Relaxed));
}
fn main() {
    match std::env::args().nth(1).as_deref() { Some("verify")=>verify(),Some("bench")=>benchmark(&[0,1,24,100,10_000],40),Some("bench-small")=>benchmark(&[1,24],400),Some("bench-large")=>benchmark(&[10_000],40),_=>panic!("expected verify or bench") }
}
