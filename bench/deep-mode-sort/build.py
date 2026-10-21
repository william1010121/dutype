from pathlib import Path
import argparse, hashlib, json, os, subprocess
parser = argparse.ArgumentParser(description="Build only: exact-source bounded sort_tree microbenchmark")
parser.add_argument('--repo', type=Path, default=Path(__file__).resolve().parents[2])
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--before', default='4baa74f2f15c51507e128056fd4447ed5d7d4523')
parser.add_argument('--after', default='1476a87f75fad5ed4a7b46ddbd6836b3eee5ad4f')
parser.add_argument('--rustc', default='rustc')
args = parser.parse_args()
repo, out = args.repo.resolve(), args.output.resolve()
out.mkdir(parents=True, exist_ok=False)
def source(revision, path):
    return subprocess.check_output(['git', 'show', f'{revision}:{path}'], cwd=repo, text=True)
scan = source(args.after, 'src/scan.rs')
category = source(args.after, 'src/category.rs')
assert scan == source(args.before, 'src/scan.rs'), 'scan implementations must match'
assert category == source(args.before, 'src/category.rs'), 'category implementations must match'

category_decl = category[category.index('/// Index into the category registry.'):category.index('\n#[derive', category.index('pub struct Category(u8);'))]
category_index = category[category.index('    pub fn index(self) -> usize {'):category.index('\n    }', category.index('    pub fn index(self) -> usize {')) + 6]
node = scan[scan.index('pub struct Node {'):scan.index('/// The filesystem of a scan root')]
prelude = '''#![allow(dead_code)]
use std::hint::black_box;
use std::time::Instant;
use std::hash::{Hash, Hasher};
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Category(u8);
impl Category { fn index(self) -> usize { self.0 as usize } }
struct Registry;
impl Registry { fn len(&self) -> usize { 1 } }
fn reg() -> Registry { Registry }
thread_local! { static FRAME_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
fn frame_peak(capacity: usize) { FRAME_PEAK.with(|v| v.set(v.get().max(capacity * std::mem::size_of::<std::slice::IterMut<'_, Node>>()))); }
'''
prelude = prelude.replace('#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]\npub struct Category(u8);\nimpl Category { fn index(self) -> usize { self.0 as usize } }', category_decl + '\nimpl Category {\n' + category_index + '\n}')
# All sorting and node/accounting implementations are extracted verbatim from
# actual source. Registry is a one-category stub used only by fixture helpers.
harness = r'''
fn file(name: String, size: u64, clone: bool) -> Node {
    Node { name: name.into(), size, cat: Category(0), special: false, hardlink: false, lossy: false, clone, dir: None }
}
fn dir(name: String, children: Vec<Node>) -> Node {
    let size = children.iter().map(|c| c.size).sum();
    let files = children.iter().map(Node::files).sum();
    let clone = children.iter().any(|c| c.clone);
    let dup = children.iter().map(|c| c.size - c.dedup()).sum();
    Node { name: name.into(), size, cat: Category(0), special: false, hardlink: false, lossy: false, clone,
        dir: Some(Box::new(DirInfo { cats: vec![size].into_boxed_slice(), files, errors: 0, children, free_base: size,
            clones: clone.then(|| Box::new(CloneInfo { dup, ..CloneInfo::default() })) })) }
}
fn group(name: String, width: usize, clone: bool) -> Node {
    let mut node = dir(name, (0..width).map(|i| file(format!("f{i:08}"), ((i % 7) * 4096) as u64, clone && i % 5 == 0)).collect());
    if clone {
        let d = node.dir.as_mut().unwrap();
        d.clones_mut().shared = (0..width).filter(|i| i % 5 == 0).map(|i| Shared {
            idx: i as u32, key: (1, i as u64), is_clone: true, ino: i as u64,
            nlink: 1, refcnt: 2, dup: i % 10 == 0,
            reflink: (i % 10 != 0).then(|| Box::new(Reflink { extents: vec![(1024, 1024)].into_boxed_slice(), dup: 1024, opaque: false }))
        }).collect();
        d.clones_mut().dup = (0..width).map(|i| d.children[i].size - d.child_dedup(i)).sum();
    }
    node
}
fn fixture(case: &str) -> Node {
    match case {
        "ordinary" => dir("ordinary".into(), (0..16).map(|i| group(format!("d{i:08}"), 16, true)).collect()),
        "nonclone" => group("nonclone".into(), 1000, false),
        "wide-files" => group("wide-files".into(), 50000, true),
        "wide-dirs" => dir("wide-dirs".into(), (0..1000).map(|i| group(format!("d{i:08}"), 8, true)).collect()),
        "deep" => {
            let mut node = group("leaf".into(), 8, true);
            for _ in 1..1000 { node = dir("d".into(), vec![node]); }
            node
        }
        _ => panic!("unknown case"),
    }
}
fn digest(node: &Node) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut pending = vec![node];
    while let Some(n) = pending.pop() {
        (&n.name, n.size, n.cat, n.special, n.hardlink, n.lossy, n.clone, n.dedup(), n.freeable()).hash(&mut h);
        if let Some(d) = &n.dir {
            (&d.cats, d.files, d.errors, d.free_base, d.children.len()).hash(&mut h);
            d.clones.is_some().hash(&mut h);
            if let Some(c) = &d.clones {
                (c.group_free, c.dup, c.gaps).hash(&mut h);
                for s in &c.shared {
                    (s.idx,s.key,s.is_clone,s.ino,s.nlink,s.refcnt,s.dup).hash(&mut h);
                    s.reflink.is_some().hash(&mut h);
                    if let Some(r) = &s.reflink { (&r.extents,r.dup,r.opaque).hash(&mut h); }
                }
            }
            for (i,child) in d.children.iter().enumerate() {
                (d.child_dedup(i),d.child_freeable(i)).hash(&mut h);
                pending.push(child);
            }
        }
    }
    h.finish()
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "layout") {
        println!("{{\"category\":{},\"node\":{},\"dir_info\":{},\"shared\":{},\"iterator\":{}}}", std::mem::size_of::<Category>(), std::mem::size_of::<Node>(), std::mem::size_of::<DirInfo>(), std::mem::size_of::<Shared>(), std::mem::size_of::<std::slice::IterMut<'_, Node>>());
        return;
    }
    let case = &args[1];
    let mode = &args[2];
    let mut root = fixture(case);
    let before = digest(&root);
    sort_tree(&mut root, true);
    let dedup = digest(&root);
    sort_tree(&mut root, false);
    let du = digest(&root);
    if mode == "time" {
        let batches = match case.as_str() { "nonclone" => 100000, "ordinary" => 100, "wide-files" => 2, "wide-dirs" => 5, "deep" => 10, _ => unreachable!() };
        let mut samples = Vec::new();
        for run in 0..18 {
            let start = Instant::now();
            for _ in 0..batches {
                sort_tree(black_box(&mut root), true);
                sort_tree(black_box(&mut root), false);
            }
            let elapsed = start.elapsed().as_nanos();
            if run >= 3 { samples.push(elapsed as f64 / batches as f64); }
        }
        assert_eq!(digest(&root), du);
        println!("{{\"before\":{before},\"dedup\":{dedup},\"du\":{du},\"pairs_per_sample\":{batches},\"samples_ns\":{samples:?}}}");
    } else {
        let bytes = FRAME_PEAK.with(|v| v.get());
        println!("{{\"before\":{before},\"dedup\":{dedup},\"du\":{du},\"peak_iterator_capacity_bytes\":{bytes}}}");
    }
}
'''
env = os.environ.copy()
rustc = args.rustc
manifest = {'category_sha256': hashlib.sha256(category.encode()).hexdigest(), 'category_decl_sha256': hashlib.sha256(category_decl.encode()).hexdigest(), 'category_index_sha256': hashlib.sha256(category_index.encode()).hexdigest(), 'scan_sha256': hashlib.sha256(scan.encode()).hexdigest(), 'sources': {}, 'stub': 'One-category Registry used only outside sorting; Node and DirInfo/accounting/sort methods copied verbatim.', 'flags': ['-C','opt-level=3','-C','lto=true','-C','codegen-units=1','--edition=2024']}
for variant in ['before', 'after', 'frames']:
    app = source(args.before if variant == 'before' else args.after, 'src/app.rs')
    sort = app[app.index('fn sort_tree('):app.index('\nfn step(')]
    manifest['sources'][variant] = {'app_sha256': hashlib.sha256(app.encode()).hexdigest(), 'sort_sha256': hashlib.sha256(sort.encode()).hexdigest()}
    if variant == 'frames':
        sort = sort.replace('let mut stack = vec![d.children.iter_mut()];', 'let mut stack = vec![d.children.iter_mut()];\n    frame_peak(stack.capacity());')
        sort = sort.replace('stack.push(d.children.iter_mut());', 'stack.push(d.children.iter_mut());\n            frame_peak(stack.capacity());')
    path = out/f'benchmark-{variant}.rs'
    path.write_text(prelude+node+sort+harness)
    command = [rustc, '+1.99.1', *manifest['flags'], str(path), '-o', str(out/f'benchmark-{variant}')]
    subprocess.run(command, env=env, check=True)
    manifest['sources'][variant]['binary_sha256'] = hashlib.sha256((out/f'benchmark-{variant}').read_bytes()).hexdigest()
manifest['layout_bytes'] = json.loads(subprocess.check_output([str(out/'benchmark-after'), 'layout'], text=True))
manifest['rustc'] = subprocess.check_output([rustc,'+1.99.1','--version','--verbose'], env=env, text=True)
manifest['source_revisions'] = {'before': args.before, 'after': args.after}
(out/'benchmark-manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
