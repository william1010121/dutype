//! Synthetic, filesystem-free release benchmark. See account-cancellation.md.
use crate::{
    account,
    category::reg,
    scan::{CloneInfo, DirInfo, Node, Reflink, Shared},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn fixture(n: usize, extents: bool) -> Node {
    let children = (0..n)
        .map(|i| Node {
            name: format!("synthetic-{:016x}", mix(i as u64)).into(),
            size: 4096,
            cat: reg().other(),
            special: false,
            hardlink: false,
            lossy: false,
            clone: true,
            dir: None,
        })
        .collect();
    let shared = (0..n)
        .map(|i| Shared {
            idx: i as u32,
            key: (1, mix((i / 2) as u64)),
            is_clone: !extents,
            ino: i as u64,
            nlink: 1,
            refcnt: 2,
            dup: false,
            reflink: extents.then(|| {
                Box::new(Reflink {
                    extents: vec![((i / 2) as u64 * 4096, 2048), ((i / 2) as u64 * 4096 + 2048, 2048)].into(),
                    dup: 0,
                    opaque: false,
                })
            }),
        })
        .collect();
    let mut cats = vec![0; reg().len()].into_boxed_slice();
    cats[reg().other().index()] = n as u64 * 4096;
    Node {
        name: "synthetic-root".into(),
        size: n as u64 * 4096,
        cat: reg().other(),
        special: false,
        hardlink: false,
        lossy: false,
        clone: true,
        dir: Some(Box::new(DirInfo {
            cats,
            files: n as u64,
            errors: 0,
            children,
            free_base: 0,
            clones: Some(Box::new(CloneInfo {
                shared,
                ..Default::default()
            })),
        })),
    }
}

// Include every node's aggregate/category result and every shared record's
// identity, refcount and duplicate credit. Computed outside operation timing.
fn fingerprint(root: &Node) -> u64 {
    fn add(h: &mut u64, v: u64) {
        for b in v.to_le_bytes() {
            *h = (*h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    fn visit(n: &Node, h: &mut u64) {
        for v in [n.size, n.dedup(), n.freeable()] {
            add(h, v);
        }
        if let Some(d) = &n.dir {
            for &v in &d.cats {
                add(h, v);
            }
            for (i, c) in d.children.iter().enumerate() {
                add(h, d.child_dedup(i));
                add(h, d.child_freeable(i));
                visit(c, h);
            }
            for s in d.shared() {
                for v in [
                    s.idx as u64,
                    s.key.0,
                    s.key.1,
                    s.ino,
                    s.nlink as u64,
                    s.refcnt as u64,
                    u64::from(s.dup),
                ] {
                    add(h, v);
                }
                if let Some(r) = &s.reflink {
                    add(h, r.dup);
                }
            }
        }
    }
    let mut h = 0xcbf29ce484222325;
    visit(root, &mut h);
    h
}

fn cancellable(root: &mut Node, flag: &AtomicBool) -> bool {
    account::account_cancellable(root, None, flag)
}

#[test]
#[ignore = "manual synthetic release benchmark"]
fn synthetic_account_cancellation() {
    let delay_ms = std::env::var("DUTYPE_CANCEL_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let later = std::env::var_os("DUTYPE_LATER_CANCEL_ONLY").is_some();
    for n in [100_000, 1_000_000] {
        for extents in [false, true] {
            for mode in ["ordinary", "scan", "cancel"] {
                if later && (n != 1_000_000 || !extents || mode != "cancel") {
                    continue;
                }
                for trial in 0..5 {
                    let mut root = fixture(n, extents);
                    let flag = Arc::new(AtomicBool::new(false));
                    let (tx, rx) = std::sync::mpsc::channel();
                    let setter = (mode == "cancel").then(|| {
                        let flag = flag.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(delay_ms));
                            let at = Instant::now();
                            flag.store(true, Ordering::Relaxed);
                            tx.send(at).unwrap();
                        })
                    });
                    let start = Instant::now();
                    let complete = if mode == "ordinary" {
                        account::account(&mut root, None);
                        true
                    } else {
                        cancellable(&mut root, &flag)
                    };
                    let returned = Instant::now();
                    let cancel_ms = if let Some(setter) = setter {
                        let at = rx.recv().unwrap();
                        setter.join().unwrap();
                        returned.saturating_duration_since(at).as_secs_f64() * 1000.
                    } else {
                        0.
                    };
                    if complete {
                        assert_eq!(root.dedup(), n as u64 * 2048);
                        assert_eq!(root.freeable(), if extents { 0 } else { n as u64 * 2048 });
                    }
                    let fingerprint = if complete { fingerprint(&root) } else { 0 };
                    let drop_start = Instant::now();
                    drop(root);
                    println!(
                        "ACCOUNT n={n} extents={extents} mode={mode} delay_ms={delay_ms} trial={trial} complete={complete} fingerprint={fingerprint:016x} op_ms={:.3} cancel_ms={cancel_ms:.3} drop_ms={:.3}",
                        returned.duration_since(start).as_secs_f64() * 1000.,
                        drop_start.elapsed().as_secs_f64() * 1000.
                    );
                }
            }
        }
    }
}
