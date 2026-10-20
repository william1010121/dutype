use super::tests::{same, temp};
use super::*;

fn leaf(name: &str, size: u64, category: usize) -> Node {
    Node {
        name: name.into(),
        size,
        cat: Category::from_index(category).unwrap(),
        special: false,
        hardlink: false,
        lossy: false,
        clone: false,
        dir: None,
    }
}

fn directory(children: Vec<Node>) -> Node {
    let mut node = leaf("d", 3, 0);
    let mut cats = vec![0; reg().len()].into_boxed_slice();
    cats[0] = node.size;
    let (mut files, mut errors, mut gaps, mut free) = (0, 0, 0, node.size);
    for child in &children {
        node.size += child.size;
        node.hardlink |= child.hardlink;
        node.clone |= child.clone;
        files += child.files();
        errors += child.errors();
        gaps += child.clone_gaps();
        for (a, b) in cats.iter_mut().zip(child.cats()) {
            *a += b;
        }
        free += child.dir.as_ref().map_or_else(|| child.freeable(), |d| d.free_base);
    }
    node.dir = Some(Box::new(DirInfo {
        cats,
        files,
        errors,
        children,
        free_base: free,
        clones: (gaps > 0).then(|| {
            Box::new(CloneInfo {
                gaps,
                ..CloneInfo::default()
            })
        }),
    }));
    node
}

/// The number of directories includes the root; files use no directory frame.
fn chain(directories: usize) -> Node {
    let mut node = leaf("f", 7, 0);
    for _ in 0..directories {
        node = directory(vec![node]);
    }
    node
}

fn snapshot(root: Node) -> Snapshot {
    Snapshot {
        root,
        cached_at: Some(UNIX_EPOCH),
        scan_time: Duration::from_millis(123),
        errors: 0,
        root_dev: None,
        clones: false,
        clone_ids: false,
        clone_gaps: false,
        private: false,
        private_sizes: false,
        private_missing: false,
        privates: PrivateSizes::default(),
        reflink_fs: None,
    }
}

fn encode(node: &Node) -> Vec<u8> {
    let mut data = Vec::new();
    Writer(&mut data).node(node).unwrap();
    data
}

fn reader(data: &[u8], clones: bool, extents: bool) -> Reader<'_> {
    Reader {
        data,
        pos: 0,
        clones,
        extents,
    }
}

fn decode(data: &[u8], clones: bool, extents: bool) -> Option<Node> {
    let mut r = reader(data, clones, extents);
    let node = r.node()?;
    (r.pos == data.len()).then_some(node)
}

fn metadata_tree() -> Node {
    let mut a = leaf("a", 7, 1);
    (a.hardlink, a.clone, a.lossy) = (true, true, true);
    let mut empty = directory(Vec::new());
    empty.special = true;
    let info = empty.dir.as_mut().unwrap();
    info.errors = 2;
    info.clones_mut().gaps = 1;
    let mut b = leaf("b", 5, 0);
    b.clone = true;
    let mut root = directory(vec![a, empty, b]);
    let info = root.dir.as_mut().unwrap();
    info.errors += 4;
    let clones = info.clones_mut();
    clones.gaps += 2;
    clones.shared = vec![
        Shared {
            idx: 0,
            key: (1, 2),
            is_clone: true,
            ino: 2,
            nlink: 2,
            refcnt: 1,
            dup: false,
            reflink: None,
        },
        Shared {
            idx: 2,
            key: (1, 3),
            is_clone: false,
            ino: 3,
            nlink: 1,
            refcnt: 1,
            dup: false,
            reflink: Some(Box::new(Reflink {
                extents: Box::new([(8, 2), (16, 1)]),
                dup: 0,
                opaque: true,
            })),
        },
    ]
    .into_boxed_slice();
    root
}

#[test]
fn v9_node_bytes_are_unchanged() {
    // Frozen pre-iterative v9 layout: directory header, three child nodes,
    // directory footer, then clone and extent records. Includes all node flags.
    let old = [
        1, b'd', 18, 0, 21, 3, 1, b'a', 7, 1, 28, 1, b'd', 3, 0, 3, 0, 2, 1, 0, 1, b'b', 5, 0, 16, 4, 2, 2, 0, 1, 2, 1,
        2, 2, 1, 0, 2, 1, 3, 0, 3, 1, 1, 2, 8, 2, 16, 1, 1,
    ];
    let node = metadata_tree();
    assert_eq!(encode(&node), old);
    same(&node, &decode(&old, true, true).unwrap());
}

#[test]
fn deep_metadata_round_trip_uses_full_cache_path() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            let out = temp("deep-metadata-out");
            let file = out.join("c.bin");
            let root_path = Path::new("synthetic-root");
            for depth in [257, 350] {
                let mut node = metadata_tree();
                let mut path = PathBuf::new();
                for _ in 1..depth {
                    node = directory(vec![node]);
                    path.push("d");
                }
                path.push("b");
                let mut snap = snapshot(node);
                (snap.clones, snap.clone_ids, snap.clone_gaps) = (true, true, true);
                (snap.private, snap.private_sizes) = (true, true);
                snap.privates.0.insert(path, 2);
                snap.reflink_fs = Some(ReflinkFs::Xfs);
                crate::account::account(&mut snap.root, Some(root_path));
                write_to(&file, root_path, &snap).unwrap();
                let back = read_from(&file, root_path).expect("deep cache must load");
                same(&snap.root, &back.root);
                assert_eq!(back.privates, snap.privates);
                assert_eq!(back.cached_at, snap.cached_at);
                assert_eq!(back.scan_time, snap.scan_time);
                assert!(back.clones && back.clone_ids && back.clone_gaps && back.private && back.private_sizes);
                assert_eq!(back.reflink_fs, snap.reflink_fs);
            }
            fs::remove_dir_all(out).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn directory_budget_and_partial_cleanup_on_small_stack() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            for depth in [0, 1, 257, 350, MAX_DIRECTORY_FRAMES] {
                let node = chain(depth);
                let data = encode(&node);
                let back = decode(&data, false, false).unwrap();
                same(&node, &back);
                drop(back);
                drop(node);
                // Late truncations destroy already assembled deep child subtrees;
                // middle truncation destroys unfinished independent parent frames.
                for end in [data.len() / 2, data.len() - 1, data.len() - 2, data.len() - 3] {
                    assert!(decode(&data[..end], false, false).is_none());
                }
            }
            let mut empty = directory(Vec::new());
            for _ in 1..MAX_DIRECTORY_FRAMES {
                empty = directory(vec![empty]);
            }
            same(&empty, &decode(&encode(&empty), false, false).unwrap());
            drop(empty);
            // Also reject an empty directory at frame limit + 1, not just a chain
            // with a file whose node depth happens to exceed the directory budget.
            for empty in [false, true] {
                let mut node = if empty { directory(Vec::new()) } else { chain(1) };
                for _ in 0..MAX_DIRECTORY_FRAMES {
                    node = directory(vec![node]);
                }
                assert_eq!(
                    Writer(io::sink()).node(&node).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                // Bypass only the writer's traversal for a decoder-boundary fixture.
                let mut data = Vec::new();
                let mut w = Writer(&mut data);
                let mut current = &node;
                let mut dirs = Vec::new();
                loop {
                    w.node_header(current).unwrap();
                    if let Some(d) = &current.dir {
                        dirs.push(d);
                        if let Some(child) = d.children.first() {
                            current = child;
                            continue;
                        }
                    }
                    break;
                }
                for d in dirs.into_iter().rev() {
                    w.directory_footer(d).unwrap();
                }
                assert!(decode(&data, false, false).is_none());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn depth_limit_failure_keeps_previous_snapshot_and_cleans_temp() {
    let out = temp("depth-limit-out");
    let file = out.join("c.bin");
    let root_path = Path::new("synthetic-root");
    write_to(&file, root_path, &snapshot(chain(1))).unwrap();
    let before = fs::read(&file).unwrap();
    let error = write_to(&file, root_path, &snapshot(chain(MAX_DIRECTORY_FRAMES + 1))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&file).unwrap(), before);
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    assert!(read_from(&file, root_path).is_some());
    fs::remove_dir_all(out).unwrap();
}

#[cfg(unix)]
#[test]
fn scanned_deep_directories_round_trip() {
    use crate::scan::{Progress, scan_excluding};
    for depth in [257, 350] {
        let src = temp(&format!("real-depth-{depth}"));
        let out = temp(&format!("real-depth-{depth}-out"));
        let mut path = src.clone();
        for _ in 1..depth {
            path.push("d");
            fs::create_dir(&path).unwrap();
        }
        fs::write(path.join("f"), b"deep cache").unwrap();
        let snap = snapshot(scan_excluding(&src, true, false, &Progress::default(), None).unwrap());
        let file = out.join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        same(&snap.root, &read_from(&file, &src).unwrap().root);
        fs::remove_file(path.join("f")).unwrap();
        for _ in 0..depth {
            fs::remove_dir(&path).unwrap();
            path.pop();
        }
        fs::remove_dir_all(out).unwrap();
    }
}

/// A bounded recursive traversal oracle. It intentionally shares field parsing
/// and accounting; the frozen v9 fixture separately checks the wire layout.
fn recursive_node(r: &mut Reader<'_>, depth: usize) -> Option<Node> {
    if depth > 32 {
        return None;
    }
    let (node, count) = r.node_header()?;
    match count {
        Some(count) => {
            let mut children = Vec::new();
            for _ in 0..count {
                children.push(recursive_node(r, depth + 1)?);
            }
            r.finish_directory(node, children)
        }
        None => Some(node),
    }
}

#[test]
fn bounded_traversals_agree_on_malformed_and_truncated_nodes() {
    for node in [chain(8), metadata_tree()] {
        let good = encode(&node);
        for end in 0..good.len() {
            assert!(decode(&good[..end], true, true).is_none());
        }
        // Exhaustive single-byte substitutions in a small valid fixture, with
        // feature-flag combinations exercising the same established checks.
        for (clones, extents) in [(false, false), (true, false), (true, true)] {
            for at in 0..good.len() {
                for byte in [0, 1, 31, 32, 127, 128, 255] {
                    let mut data = good.clone();
                    data[at] = byte;
                    let mut old = reader(&data, clones, extents);
                    let before = recursive_node(&mut old, 0).filter(|_| old.pos == data.len());
                    let after = decode(&data, clones, extents);
                    assert_eq!(before.is_some(), after.is_some(), "offset {at}, byte {byte}");
                    if let (Some(before), Some(after)) = (before, after) {
                        same(&before, &after);
                    }
                }
            }
        }
    }
}
