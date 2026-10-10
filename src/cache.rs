//! On-disk cache of scan results (`--cache`), so a big tree opens instantly
//! the second time. One file per (path, --cross-fs) in ~/.cache/dutype.
//! A cache is never trusted across dutype versions or category rule changes.

use crate::category::{Category, reg};
use crate::scan::{DirInfo, Node};
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// v4: directories store their own unreadable-entry count; flag bit 2 marks
// hard-linked files, bit 3 lossy (non-UTF-8) names.
const MAGIC: &[u8; 8] = b"DUTYPE\0\x04";

pub struct Snapshot {
    pub root: Node,
    /// When the scan ran; `None` for a scan that was not loaded from the cache.
    pub cached_at: Option<SystemTime>,
    pub scan_time: Duration,
    pub errors: u64,
}

pub fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = if seed == 0 { 0xcbf2_9ce4_8422_2325 } else { seed };
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// `$DUTYPE_CACHE_DIR`, else the OS cache directory plus `dutype`.
pub fn dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DUTYPE_CACHE_DIR") {
        return Some(PathBuf::from(p));
    }
    let env = |k| std::env::var_os(k).map(PathBuf::from);
    // Like the config (~/.config/dutype): one global place, also on macOS.
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|b| b.join("dutype").join("cache"))
    } else {
        env("XDG_CACHE_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".cache")))
            .map(|b| b.join("dutype"))
    }
}

/// Create the cache directory up front. Called before any scan when caching
/// is on: creating it later could grow its parent's allocated blocks after
/// they were measured, making the first and later scans disagree.
pub fn prepare() -> io::Result<()> {
    match dir() {
        Some(d) => platform::create_private_dir(&d),
        None => Ok(()),
    }
}

/// Snapshots list every file and directory name below the scanned root, so
/// they are private to the user (0600 files, 0700 directories on unix).
mod platform {
    use std::fs::{DirBuilder, File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub fn create_private_dir(dir: &Path) -> io::Result<()> {
        let mut b = DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(dir)
    }

    pub fn create_private_file(path: &Path) -> io::Result<File> {
        let mut o = OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(path)
    }
}

fn file_for(root: &Path, one_fs: bool) -> Option<PathBuf> {
    let h = fnv1a(0, root.as_os_str().as_encoded_bytes());
    Some(dir()?.join(format!("{h:016x}-{}.bin", if one_fs { "x" } else { "all" })))
}

/// Whether `root` is the cache directory or inside it. Such scans are never
/// cached: saving would change the tree that was just measured.
fn inside_cache_dir(root: &Path) -> bool {
    dir()
        .and_then(|d| fs::canonicalize(d).ok())
        .is_some_and(|d| root.starts_with(d))
}

/// The cached scan of `root`, if there is a valid one.
pub fn load(root: &Path, one_fs: bool) -> Option<Snapshot> {
    if inside_cache_dir(root) {
        return None;
    }
    read_from(&file_for(root, one_fs)?, root)
}

/// Save a scan. Returns `Ok(None)` without writing when `root_path` is the
/// cache directory or inside it.
pub fn save(root_path: &Path, one_fs: bool, snap: &Snapshot) -> io::Result<Option<PathBuf>> {
    if inside_cache_dir(root_path) {
        return Ok(None);
    }
    let file = file_for(root_path, one_fs).ok_or_else(|| io::Error::other("no cache directory"))?;
    platform::create_private_dir(file.parent().unwrap())?;
    write_to(&file, root_path, snap)?;
    Ok(Some(file))
}

fn write_to(file: &Path, root_path: &Path, snap: &Snapshot) -> io::Result<()> {
    // Write a sibling and rename, so readers never see a half-written file.
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut w = Writer(BufWriter::with_capacity(1 << 20, platform::create_private_file(&tmp)?));
        w.0.write_all(MAGIC)?;
        w.0.write_all(&reg().fingerprint.to_le_bytes())?;
        let at = snap.cached_at.unwrap_or_else(SystemTime::now);
        w.varint(at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))?;
        w.varint(snap.scan_time.as_millis() as u64)?;
        w.varint(snap.errors)?;
        w.bytes(root_path.as_os_str().as_encoded_bytes())?;
        w.node(&snap.root)?;
        w.0.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    match result {
        Ok(()) => fs::rename(&tmp, file),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn read_from(file: &Path, root_path: &Path) -> Option<Snapshot> {
    let data = fs::read(file).ok()?;
    let mut r = Reader { data: &data, pos: 0 };
    if r.take(MAGIC.len())? != MAGIC || u64::from_le_bytes(r.take(8)?.try_into().ok()?) != reg().fingerprint {
        return None;
    }
    let at = UNIX_EPOCH.checked_add(Duration::from_secs(r.varint()?))?;
    let scan_time = Duration::from_millis(r.varint()?);
    let errors = r.varint()?;
    // A hash collision between two paths must not show the wrong tree.
    if r.bytes()? != root_path.as_os_str().as_encoded_bytes() {
        return None;
    }
    let root = r.node(0)?;
    (r.pos == data.len()).then_some(Snapshot {
        root,
        cached_at: Some(at),
        scan_time,
        errors,
    })
}

struct Writer<W: Write>(W);

impl<W: Write> Writer<W> {
    fn varint(&mut self, mut v: u64) -> io::Result<()> {
        let mut buf = [0u8; 10];
        let mut n = 0;
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            buf[n] = byte | if v == 0 { 0 } else { 0x80 };
            n += 1;
            if v == 0 {
                return self.0.write_all(&buf[..n]);
            }
        }
    }

    fn bytes(&mut self, b: &[u8]) -> io::Result<()> {
        self.varint(b.len() as u64)?;
        self.0.write_all(b)
    }

    /// Pre-order: name, size, category, flags, then for dirs the children and
    /// the dir's own error count. Per-dir category totals, file counts and
    /// error totals are derived again on load.
    fn node(&mut self, n: &Node) -> io::Result<()> {
        self.bytes(n.name.as_bytes())?;
        self.varint(n.size)?;
        self.varint(n.cat.index() as u64)?;
        self.0
            .write_all(&[n.is_dir() as u8 | (n.special as u8) << 1 | (n.hardlink as u8) << 2 | (n.lossy as u8) << 3])?;
        if let Some(d) = &n.dir {
            self.varint(d.children.len() as u64)?;
            for c in &d.children {
                self.node(c)?;
            }
            let child_errors: u64 = d.children.iter().map(Node::errors).sum();
            self.varint(d.errors.saturating_sub(child_errors))?;
        }
        Ok(())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn bytes(&mut self) -> Option<&[u8]> {
        let n = self.varint()?;
        self.take(usize::try_from(n).ok()?)
    }

    fn node(&mut self, depth: usize) -> Option<Node> {
        // Deeper than any sane tree, and keeps the recursion well within
        // small (2 MiB) thread stacks even in debug builds. Deeper snapshots
        // are just a cache miss.
        if depth > 256 {
            return None;
        }
        let name: Box<str> = std::str::from_utf8(self.bytes()?).ok()?.into();
        let size = self.varint()?;
        let cat = Category::from_index(usize::try_from(self.varint()?).ok()?)?;
        let flags = *self.take(1)?.first()?;
        let mut hardlink_flag = flags & 4 != 0;
        let dir = if flags & 1 != 0 {
            let count = usize::try_from(self.varint()?).ok()?;
            // `count` is untrusted. Every child takes several bytes, so a count
            // beyond the remaining data is corrupt; and nothing is reserved from
            // it (not even capped: nested dirs would add up), so memory only
            // grows with children actually decoded.
            if count > self.data.len() - self.pos {
                return None;
            }
            let mut children = Vec::new();
            let mut cats = vec![0u64; reg().len()].into_boxed_slice();
            // Checked arithmetic throughout: a damaged file is a miss, never
            // a panic or a wrapped (wrong) total.
            let (mut files, mut child_bytes, mut errors) = (0u64, 0u64, 0u64);
            let mut hardlink = false;
            for _ in 0..count {
                let c = self.node(depth + 1)?;
                child_bytes = child_bytes.checked_add(c.size)?;
                files = files.checked_add(c.files())?;
                errors = errors.checked_add(c.errors())?;
                hardlink |= c.hardlink;
                match &c.dir {
                    Some(d) => {
                        for (a, b) in cats.iter_mut().zip(d.cats.iter()) {
                            *a = a.checked_add(*b)?;
                        }
                    }
                    None => cats[c.cat.index()] = cats[c.cat.index()].checked_add(c.size)?,
                }
                children.push(c);
            }
            // The directory's own blocks, as in `scan_dir`.
            cats[cat.index()] = cats[cat.index()].checked_add(size.checked_sub(child_bytes)?)?;
            let errors = errors.checked_add(self.varint()?)?;
            hardlink_flag = hardlink;
            Some(Box::new(DirInfo {
                cats,
                files,
                errors,
                children,
            }))
        } else {
            None
        };
        Some(Node {
            name,
            size,
            cat,
            special: flags & 2 != 0,
            hardlink: hardlink_flag,
            lossy: flags & 8 != 0,
            dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Progress, scan};

    fn same(a: &Node, b: &Node) {
        assert_eq!((&a.name, a.size, a.cat, a.special), (&b.name, b.size, b.cat, b.special));
        assert_eq!((a.files(), a.cats()), (b.files(), b.cats()), "{}", a.name);
        assert_eq!(a.children().len(), b.children().len());
        a.children().iter().zip(b.children()).for_each(|(x, y)| same(x, y));
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dutype-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cache_dir_inside_root_is_not_counted() {
        let root = fs::canonicalize(temp("excl")).unwrap();
        fs::write(root.join("a.pdf"), vec![1u8; 10_000]).unwrap();
        let cache = root.join(".cache").join("dutype");
        fs::create_dir_all(&cache).unwrap();
        let before = crate::scan::scan_excluding(&root, true, &Progress::default(), Some(&cache)).unwrap();
        fs::write(cache.join("snap.bin"), vec![2u8; 200_000]).unwrap();
        let after = crate::scan::scan_excluding(&root, true, &Progress::default(), Some(&cache)).unwrap();
        assert_eq!(before.size, after.size);
        assert_eq!(before.files(), after.files());
    }

    /// A refresh replaces an existing snapshot. `fs::rename` replaces an
    /// existing file on Windows too; CI runs this on Windows.
    #[test]
    fn save_replaces_existing_snapshot() {
        let src = fs::canonicalize(temp("replace-src")).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        let file = temp("replace-out").join("c.bin");
        let snap = |errors| Snapshot {
            root: scan(&src, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors,
        };
        write_to(&file, &src, &snap(1)).unwrap();
        write_to(&file, &src, &snap(2)).unwrap();
        assert_eq!(read_from(&file, &src).unwrap().errors, 2);
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let src = fs::canonicalize(temp("perm-src")).unwrap();
        let file = temp("perm-out").join("sub").join("c.bin");
        platform::create_private_dir(file.parent().unwrap()).unwrap();
        let snap = Snapshot {
            root: scan(&src, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
        };
        write_to(&file, &src, &snap).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
    }

    #[test]
    fn round_trip() {
        let src = temp("src");
        fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        fs::write(src.join("node_modules/pkg/index.js"), "x".repeat(5000)).unwrap();
        fs::write(src.join("movie.mp4"), vec![0u8; 20_000]).unwrap();
        fs::write(src.join("notes.pdf"), "pdf").unwrap();
        let src = fs::canonicalize(&src).unwrap();
        let root = scan(&src, true, &Progress::default()).unwrap();
        let snap = Snapshot {
            root,
            cached_at: None,
            scan_time: Duration::from_millis(1234),
            errors: 3,
        };

        let file = temp("out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        let back = read_from(&file, &src).unwrap();
        same(&snap.root, &back.root);
        assert_eq!((back.scan_time, back.errors), (snap.scan_time, 3));
        assert!(back.cached_at.is_some());

        // Child sizes whose sum overflows u64 are a miss, not a panic or a
        // wrapped total: root dir (size 5) with two files of u64::MAX bytes.
        let mut over = Vec::from(&MAGIC[..]);
        over.extend(reg().fingerprint.to_le_bytes());
        over.extend([0, 0, 0]); // cached_at, scan_time, errors
        let path = src.as_os_str().as_encoded_bytes();
        Writer(&mut over).bytes(path).unwrap();
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        over.extend([1, b'r', 5, 0, 1, 2]); // name "r", size 5, cat 0, dir, 2 children
        for _ in 0..2 {
            over.extend([1, b'f']);
            over.extend(max);
            over.extend([0, 0]); // cat 0, file
        }
        let over_file = temp("over").join("c.bin");
        fs::write(&over_file, &over).unwrap();
        assert!(read_from(&over_file, &src).is_none());

        // A huge corrupt child count is a miss, not a huge allocation.
        let mut huge = Vec::from(&MAGIC[..]);
        huge.extend(reg().fingerprint.to_le_bytes());
        huge.extend([0, 0, 0]);
        Writer(&mut huge).bytes(src.as_os_str().as_encoded_bytes()).unwrap();
        huge.extend([1, b'r', 5, 0, 1]); // dir "r"
        Writer(&mut huge).varint(u64::from(u32::MAX) * 1000).unwrap(); // children
        let huge_file = temp("huge").join("c.bin");
        fs::write(&huge_file, &huge).unwrap();
        assert!(read_from(&huge_file, &src).is_none());

        // A deep chain of dirs each claiming 4096 children (but holding one)
        // is a miss without reserving memory per level.
        let mut chain = Vec::from(&MAGIC[..]);
        chain.extend(reg().fingerprint.to_le_bytes());
        chain.extend([0, 0, 0]);
        Writer(&mut chain).bytes(src.as_os_str().as_encoded_bytes()).unwrap();
        for _ in 0..400 {
            chain.extend([1, b'd', 0, 0, 1]); // dir "d", size 0
            Writer(&mut chain).varint(4096).unwrap();
        }
        let chain_file = temp("chain").join("c.bin");
        fs::write(&chain_file, &chain).unwrap();
        assert!(read_from(&chain_file, &src).is_none());

        // A corrupt timestamp is a miss, not a panic.
        let mut bad = Vec::from(&MAGIC[..]);
        bad.extend(reg().fingerprint.to_le_bytes());
        bad.extend([0xff; 9]);
        bad.push(0x01); // u64::MAX seconds, beyond SystemTime on some platforms
        let bad_file = temp("bad").join("c.bin");
        fs::write(&bad_file, &bad).unwrap();
        assert!(read_from(&bad_file, &src).is_none());

        // Another path, or a truncated file, is a miss rather than a wrong tree.
        assert!(read_from(&file, &src.join("node_modules")).is_none());
        let data = fs::read(&file).unwrap();
        fs::write(&file, &data[..data.len() - 1]).unwrap();
        assert!(read_from(&file, &src).is_none());
    }
}
