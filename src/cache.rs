//! On-disk cache of scan results (`--cache`), so a big tree opens instantly
//! the second time. One file per (path, --cross-fs) in ~/.cache/dutype.
//! A cache is never trusted across dutype versions or category rule changes.

use crate::category::{Category, reg};
use crate::scan::{CloneInfo, DirInfo, Extents, Node, PrivateSizes, Reflink, ReflinkFs, Shared};
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// v5: header stores the root's device; directories store their own
// unreadable-entry count; flag bit 2 marks hard-linked files, bit 3 lossy
// (non-UTF-8) names.
// v7: header flags for `--private` (bits 4 and 5); the private sizes follow
// the tree.
// v8: a byte after the header flags holds the Linux reflink filesystem whose
// shared extents were read (`--clones`); shared-block records end with
// those extents (and, if any, a byte: some shared extents had an unknown
// size).
// v9: same layout, but allocation belongs to a stable hard-link path.
// Older snapshots preserve first-arriving attribution and must be rescanned.
const MAGIC: &[u8; 8] = b"DUTYPE\0\x09";

pub struct Snapshot {
    pub root: Node,
    /// When the scan ran; `None` for a scan that was not loaded from the cache.
    pub cached_at: Option<SystemTime>,
    pub scan_time: Duration,
    pub errors: u64,
    /// Device of the root when the tree was scanned. A snapshot whose root is
    /// now on another device (remounted) is stale and not loaded.
    pub root_dev: Option<u64>,
    /// The scan looked for APFS clones (`--clones`). A snapshot without
    /// them cannot answer a request that wants them.
    pub clones: bool,
    /// The scan could see APFS clone families (see `Progress::clone_ids`).
    pub clone_ids: bool,
    /// ... but not for every file (see `Progress::clone_gaps`).
    pub clone_gaps: bool,
    /// The scan was made with `--private` (see `Progress::private`).
    pub private: bool,
    /// A volume returned the flags that pick out partial clones (see
    /// `Progress::private_sizes`).
    pub private_sizes: bool,
    /// ... but some partial clone's private size was not reported (see
    /// `Progress::private_missing`): the table is then incomplete and not
    /// offered.
    pub private_missing: bool,
    /// The private sizes themselves (empty without `--private`).
    pub privates: PrivateSizes,
    /// With `clones`, the Linux reflink filesystem whose shared extents were
    /// read (see `Progress::reflink_fs`).
    pub reflink_fs: Option<ReflinkFs>,
}

impl Snapshot {
    /// Whether this snapshot can answer a request with or without `--private`.
    pub fn serves(&self, private: bool) -> bool {
        self.private || !private
    }

    /// Its private sizes, if the scan read them.
    pub fn private_table(&self) -> Option<&PrivateSizes> {
        (self.private_sizes && !self.private_missing).then_some(&self.privates)
    }
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

    /// Create a new file exclusively: fails if anything (including a
    /// symlink planted by someone else) already exists at `path`.
    pub fn create_private_file(path: &Path) -> io::Result<File> {
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(path)
    }

    /// Owned by the current user and not writable by group or others, so
    /// nobody else could have written it. A missing path is not private.
    #[cfg(unix)]
    pub fn is_private(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        std::fs::symlink_metadata(path).is_ok_and(|m| m.uid() == me && m.mode() & 0o022 == 0)
    }

    #[cfg(not(unix))]
    pub fn is_private(path: &Path) -> bool {
        path.exists()
    }

    /// Before writing: the cache directory must belong to the current user;
    /// one owned by someone else is refused. Our own directory is tightened
    /// to 0700 if group or others have access (e.g. created under umask 002),
    /// so nobody else can read or plant entries.
    #[cfg(unix)]
    pub fn check_private_dir(dir: &Path, _own_file: &Path) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        let d = std::fs::symlink_metadata(dir)?;
        if !d.is_dir() || d.uid() != me {
            return Err(io::Error::other(format!(
                "refusing cache directory {}: not a directory owned by you",
                dir.display()
            )));
        }
        if d.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn check_private_dir(_: &Path, _: &Path) -> io::Result<()> {
        Ok(())
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

/// The cached scan of `root`, if there is a valid one (and, if `clones`
/// is asked for, one that looked for APFS clones).
pub fn load(root: &Path, one_fs: bool, clones: bool) -> Option<Snapshot> {
    if inside_cache_dir(root) {
        return None;
    }
    let file = file_for(root, one_fs)?;
    // Only trust snapshots nobody else could have written (read-only check:
    // permissions are tightened on the next save, not here).
    if !platform::is_private(file.parent()?) || !platform::is_private(&file) {
        return None;
    }
    read_from(&file, root).filter(|snap| same_device(snap, root) && answers(snap, clones))
}

/// The snapshot can answer a request with or without `--clones`: one that
/// did not look for clones cannot answer one that wants them.
pub fn answers(snap: &Snapshot, clones: bool) -> bool {
    snap.clones || !clones
}

/// The snapshot's root is still on the device it was scanned on (a root
/// remounted since describes another filesystem).
fn same_device(snap: &Snapshot, root: &Path) -> bool {
    snap.root_dev.is_none() || snap.root_dev == crate::scan::device_of(root).ok()
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
    // The temp name is unpredictable and created exclusively (never through
    // an existing file or symlink), and the directory must be private.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let tmp = file.with_extension(format!("tmp{}-{nonce:x}", std::process::id()));
    let out = platform::create_private_file(&tmp)?;
    let result = (|| {
        platform::check_private_dir(file.parent().unwrap_or(Path::new(".")), &tmp)?;
        let mut w = Writer(BufWriter::with_capacity(1 << 20, out));
        w.0.write_all(MAGIC)?;
        w.0.write_all(&reg().fingerprint.to_le_bytes())?;
        let at = snap.cached_at.unwrap_or_else(SystemTime::now);
        w.varint(at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))?;
        w.varint(snap.scan_time.as_millis() as u64)?;
        w.varint(snap.errors)?;
        w.0.write_all(&[snap.root_dev.is_some() as u8
            | (snap.clone_ids as u8) << 1
            | (snap.clones as u8) << 2
            | (snap.clone_gaps as u8) << 3
            | (snap.private as u8) << 4
            | (snap.private_sizes as u8) << 5
            | (snap.private_missing as u8) << 6])?;
        w.0.write_all(&[snap.reflink_fs.map_or(0, ReflinkFs::code)])?;
        w.varint(snap.root_dev.unwrap_or(0))?;
        w.bytes(root_path.as_os_str().as_encoded_bytes())?;
        w.node(&snap.root)?;
        // An incomplete table is not kept (see `private_table`).
        w.private(snap.private_table().unwrap_or(&PrivateSizes::default()))?;
        w.0.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    match result {
        Ok(()) => fs::rename(&tmp, file).inspect_err(|_| {
            // Each attempt uses a new temp name; never leave a full copy behind.
            let _ = fs::remove_file(&tmp);
        }),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn read_from(file: &Path, root_path: &Path) -> Option<Snapshot> {
    let data = fs::read(file).ok()?;
    let mut r = Reader {
        data: &data,
        pos: 0,
        clones: false,
        extents: false,
    };
    if r.take(MAGIC.len())? != MAGIC || u64::from_le_bytes(r.take(8)?.try_into().ok()?) != reg().fingerprint {
        return None;
    }
    let at = UNIX_EPOCH.checked_add(Duration::from_secs(r.varint()?))?;
    let scan_time = Duration::from_millis(r.varint()?);
    let errors = r.varint()?;
    let flags = *r.take(1)?.first()?;
    if flags > 127 {
        return None;
    }
    let (has_dev, clone_ids, clones) = (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0);
    let clone_gaps = flags & 8 != 0;
    r.clones = clones;
    let (private, private_sizes, private_missing) = (flags & 16 != 0, flags & 32 != 0, flags & 64 != 0);
    // Sizes are only ever collected on request.
    if (private_sizes || private_missing) && !private {
        return None;
    }
    // The reflink filesystem whose shared extents were read, only with
    // `--clones`.
    let fs = *r.take(1)?.first()?;
    let reflink_fs = ReflinkFs::from_code(fs);
    if (fs != 0 && reflink_fs.is_none()) || (reflink_fs.is_some() && !clones) {
        return None;
    }
    r.extents = reflink_fs.is_some();
    let dev = r.varint()?;
    let root_dev = has_dev.then_some(dev);
    // A hash collision between two paths must not show the wrong tree.
    if r.bytes()? != root_path.as_os_str().as_encoded_bytes() {
        return None;
    }
    let mut root = r.node(0)?;
    let privates = r.private(&root, private_sizes && !private_missing)?;
    if r.pos != data.len() {
        return None;
    }
    if clones {
        crate::account::account(&mut root, Some(root_path));
    }
    Some(Snapshot {
        root,
        cached_at: Some(at),
        scan_time,
        errors,
        root_dev,
        clones,
        clone_ids,
        clone_gaps,
        private,
        private_sizes,
        private_missing,
        privates,
        reflink_fs,
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

    /// Pre-order: name, size, category, flags, then for dirs the children,
    /// the dir's own error and clone-gap counts and its shared-block records
    /// (with their shared extents). Per-dir
    /// category totals, file counts, error totals and freeable bytes are
    /// derived again on load.
    fn node(&mut self, n: &Node) -> io::Result<()> {
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
            self.bytes(n.name.as_bytes())?;
            self.varint(n.size)?;
            self.varint(n.cat.index() as u64)?;
            self.0.write_all(&[n.is_dir() as u8
                | (n.special as u8) << 1
                | (n.hardlink as u8) << 2
                | (n.lossy as u8) << 3
                | (n.clone as u8) << 4])?;
            if let Some(d) = &n.dir {
                self.varint(d.children.len() as u64)?;
                for c in &d.children {
                    self.node(c)?;
                }
                let child_errors: u64 = d.children.iter().map(Node::errors).sum();
                self.varint(d.errors.saturating_sub(child_errors))?;
                let child_gaps: u64 = d.children.iter().map(Node::clone_gaps).sum();
                self.varint(d.clone_gaps().saturating_sub(child_gaps))?;
                self.varint(d.shared().len() as u64)?;
                for s in d.shared() {
                    for v in [
                        s.idx as u64,
                        s.key.0,
                        s.key.1,
                        s.is_clone as u64,
                        s.ino,
                        s.nlink as u64,
                        s.refcnt as u64,
                    ] {
                        self.varint(v)?;
                    }
                    let extents = s.reflink.as_ref().map_or(&[][..], |r| &r.extents);
                    self.varint(extents.len() as u64)?;
                    for &(start, len) in extents {
                        self.varint(start)?;
                        self.varint(len)?;
                    }
                    if let Some(r) = &s.reflink {
                        self.0.write_all(&[r.opaque as u8])?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Count, then per file (sorted, so equal tables give equal files): its
    /// path's names, then the bytes.
    fn private(&mut self, p: &PrivateSizes) -> io::Result<()> {
        let mut all: Vec<_> = p.0.iter().collect();
        all.sort();
        self.varint(all.len() as u64)?;
        for (path, bytes) in all {
            self.varint(path.components().count() as u64)?;
            for c in path.components() {
                self.bytes(c.as_os_str().as_encoded_bytes())?;
            }
            self.varint(*bytes)?;
        }
        Ok(())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    /// The snapshot looked for clones; without it, no dir may carry
    /// clone gaps or shared-block records (a scan never makes them).
    clones: bool,
    /// Shared extents may appear (`--clones` on a reflink filesystem).
    extents: bool,
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
            // The 10th byte holds only bit 63: anything more would be silently
            // truncated, turning a damaged size into a plausible wrong one.
            if shift == 63 && b > 1 {
                return None;
            }
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

    /// The private sizes after the tree (see [`Writer::private`]). Each must
    /// name a distinct file of `root` and be at most its size; any at all
    /// only if the header says the snapshot holds them.
    fn private(&mut self, root: &Node, allowed: bool) -> Option<PrivateSizes> {
        let count = usize::try_from(self.varint()?).ok()?;
        if count > self.data.len() - self.pos || (count > 0 && !allowed) {
            return None;
        }
        let mut p = PrivateSizes::default();
        for _ in 0..count {
            let names = usize::try_from(self.varint()?).ok()?;
            if names == 0 || names > self.data.len() - self.pos {
                return None;
            }
            let mut path = PathBuf::new();
            for _ in 0..names {
                let name = std::str::from_utf8(self.bytes()?).ok()?;
                // One plain name per component, as the tree has them (a
                // backslash is an ordinary character on unix).
                if name.is_empty() || name.contains(std::path::is_separator) || name == "." || name == ".." {
                    return None;
                }
                path.push(name);
            }
            let bytes = self.varint()?;
            if p.0.insert(path, bytes).is_some() {
                return None;
            }
        }
        if !p.0.is_empty() {
            let mut matched = 0;
            let mut ok = true;
            // As reports look sizes up: never under a lossy name.
            crate::scan::walk_with_ancestors(root, &mut PathBuf::new(), &mut Vec::new(), &mut |n, _, rel, up| {
                if !n.is_dir()
                    && !n.lossy
                    && !up.iter().any(|a| a.lossy)
                    && let Some(b) = p.get(rel)
                {
                    matched += 1;
                    ok &= b <= n.size;
                }
                ok
            });
            if !ok || matched != p.0.len() {
                return None;
            }
        }
        Some(p)
    }

    /// Shared extents of a shared-block record: non-empty ranges sorted by
    /// start, none past the end of the address space. `Some(None)` for none.
    fn extents(&mut self) -> Option<Option<Extents>> {
        let count = usize::try_from(self.varint()?).ok()?;
        if count == 0 {
            return Some(None);
        }
        // Each range takes at least two bytes.
        if !self.extents || count > (self.data.len() - self.pos) / 2 {
            return None;
        }
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(count);
        for _ in 0..count {
            let (start, len) = (self.varint()?, self.varint()?);
            if len == 0 || start.checked_add(len).is_none() || out.last().is_some_and(|&p| p > (start, len)) {
                return None;
            }
            out.push((start, len));
        }
        Some(Some(out.into_boxed_slice()))
    }

    fn node(&mut self, depth: usize) -> Option<Node> {
        // Bound recursion when decoding untrusted snapshots, including on
        // small (2 MiB) thread stacks in debug builds. Deeper real trees are
        // supported by the scanner but their snapshots are a cache miss.
        if depth > 256 {
            return None;
        }
        let name: Box<str> = std::str::from_utf8(self.bytes()?).ok()?.into();
        let size = self.varint()?;
        let cat = Category::from_index(usize::try_from(self.varint()?).ok()?)?;
        let flags = *self.take(1)?.first()?;
        if flags > 31 {
            return None;
        }
        let mut hardlink_flag = flags & 4 != 0;
        let mut clone_flag = flags & 16 != 0;
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
            let (mut files, mut child_bytes, mut errors, mut free) = (0u64, 0u64, 0u64, 0u64);
            let mut gaps = 0u64;
            let (mut hardlink, mut clone) = (false, false);
            for _ in 0..count {
                let c = self.node(depth + 1)?;
                child_bytes = child_bytes.checked_add(c.size)?;
                files = files.checked_add(c.files())?;
                errors = errors.checked_add(c.errors())?;
                gaps = gaps.checked_add(c.clone_gaps())?;
                hardlink |= c.hardlink;
                clone |= c.clone;
                match &c.dir {
                    Some(d) => {
                        for (a, b) in cats.iter_mut().zip(d.cats.iter()) {
                            *a = a.checked_add(*b)?;
                        }
                        free = free.checked_add(d.free_base)?;
                    }
                    None => {
                        cats[c.cat.index()] = cats[c.cat.index()].checked_add(c.size)?;
                        if !c.hardlink && !c.clone {
                            free = free.checked_add(c.size)?;
                        }
                    }
                }
                children.push(c);
            }
            // The directory's own blocks, as in `scan_dir`.
            let own = size.checked_sub(child_bytes)?;
            cats[cat.index()] = cats[cat.index()].checked_add(own)?;
            let errors = errors.checked_add(self.varint()?)?;
            let own_gaps = self.varint()?;
            let clone_gaps = gaps.checked_add(own_gaps)?;
            let shared_count = usize::try_from(self.varint()?).ok()?;
            if shared_count > children.len() || (!self.clones && (own_gaps > 0 || shared_count > 0)) {
                return None;
            }
            let mut shared = Vec::with_capacity(shared_count);
            for _ in 0..shared_count {
                let idx = u32::try_from(self.varint()?).ok()?;
                let key = (self.varint()?, self.varint()?);
                let is_clone = match self.varint()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let (ino, nlink) = (self.varint()?, u32::try_from(self.varint()?).ok()?);
                let refcnt = u32::try_from(self.varint()?).ok()?;
                // In order, each naming a distinct file child that shares
                // blocks (so `account` visits its dir).
                let prev = shared.last().map(|s: &Shared| s.idx as i64).unwrap_or(-1);
                let shares = |c: &Node| !c.is_dir() && (c.hardlink || c.clone);
                if idx as i64 <= prev || !children.get(idx as usize).is_some_and(shares) {
                    return None;
                }
                let extents = self.extents()?;
                // Files with shared extents are marked as possibly sharing.
                if extents.is_some() && !children[idx as usize].clone {
                    return None;
                }
                shared.push(Shared {
                    idx,
                    key,
                    is_clone,
                    ino,
                    nlink,
                    refcnt,
                    dup: false,
                    reflink: match extents {
                        Some(extents) => Some(Box::new(Reflink {
                            extents,
                            dup: 0,
                            opaque: match *self.take(1)?.first()? {
                                0 => false,
                                1 => true,
                                _ => return None,
                            },
                        })),
                        None => None,
                    },
                });
            }
            hardlink_flag = hardlink;
            clone_flag = clone;
            Some(Box::new(DirInfo {
                cats,
                files,
                errors,
                children,
                free_base: free.checked_add(own)?,
                clones: (!shared.is_empty() || clone_gaps > 0).then(|| {
                    Box::new(CloneInfo {
                        shared: shared.into_boxed_slice(),
                        gaps: clone_gaps,
                        ..CloneInfo::default()
                    })
                }),
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
            clone: clone_flag,
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
        assert_eq!(
            (a.hardlink, a.clone, a.lossy, a.dedup(), a.freeable()),
            (b.hardlink, b.clone, b.lossy, b.dedup(), b.freeable()),
            "{}",
            a.name
        );
        if let (Some(x), Some(y)) = (&a.dir, &b.dir) {
            assert_eq!(x.shared(), y.shared(), "{}", a.name);
        }
        assert_eq!(a.children().len(), b.children().len());
        a.children().iter().zip(b.children()).for_each(|(x, y)| same(x, y));
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dutype-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Clone families and hard links survive a round trip, and the
    /// deduplicated and freeable sizes are derived again on load.
    #[cfg(unix)]
    #[test]
    fn round_trip_keeps_shared_blocks() {
        let since = crate::scan::test_support::stamp();
        let src = temp("shared-src");
        fs::create_dir_all(src.join("d")).unwrap();
        fs::write(src.join("d/a"), vec![3u8; 2 << 20]).unwrap();
        fs::hard_link(src.join("d/a"), src.join("link")).unwrap();
        if cfg!(target_os = "macos") {
            for c in ["clone", "partial"] {
                let ok = std::process::Command::new("cp")
                    .arg("-c")
                    .arg(src.join("d/a"))
                    .arg(src.join(c))
                    .status();
                assert!(ok.unwrap().success());
            }
            // A partial clone, for a private size.
            let mut f = fs::OpenOptions::new().append(true).open(src.join("partial")).unwrap();
            f.write_all(&[5u8; 1 << 20]).unwrap();
            f.sync_all().unwrap();
        }
        let progress = Progress {
            private: true,
            ..Progress::default()
        };
        let root = scan(&src, true, true, &progress).unwrap();
        let snap = Snapshot {
            root,
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: progress.clone_ids.load(std::sync::atomic::Ordering::Relaxed),
            clone_gaps: true,
            private: true,
            private_sizes: progress.private_sizes.load(std::sync::atomic::Ordering::Relaxed),
            private_missing: progress.private_missing.load(std::sync::atomic::Ordering::Relaxed),
            privates: progress.take_private(),
            reflink_fs: None,
        };
        assert_eq!(snap.clone_ids, cfg!(target_os = "macos"));
        assert_eq!(snap.private_sizes, cfg!(target_os = "macos"));
        let file = temp("shared-out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        let back = read_from(&file, &src).unwrap();
        same(&snap.root, &back.root);
        assert_eq!(
            (back.clones, back.clone_ids, back.clone_gaps),
            (snap.clones, snap.clone_ids, snap.clone_gaps)
        );
        assert_eq!((back.private, back.private_sizes), (true, snap.private_sizes));
        assert!(back.serves(true) && back.serves(false));
        assert!(answers(&back, true) && answers(&back, false));
        if cfg!(target_os = "macos") {
            assert_eq!(back.root.dedup(), back.root.size - (2 << 20));
            assert_eq!(back.privates, snap.privates);
            let got = back.privates.get(Path::new("partial"));
            crate::scan::test_support::assert_private(got, 1 << 20, &since);

            // A table in a file whose header does not allow one (damaged)
            // is a miss: clear the "sizes read" bit of a good file.
            write_to(&file, &src, &snap).unwrap();
            let mut data = fs::read(&file).unwrap();
            let mut r = Reader {
                data: &data,
                pos: 16,
                clones: true,
                extents: false,
            };
            // cached_at, scan time, errors
            for _ in 0..3 {
                r.varint().unwrap();
            }
            let flags_at = r.pos;
            data[flags_at] &= !32;
            fs::write(&file, &data).unwrap();
            assert!(read_from(&file, &src).is_none());
            // Sizes without the request are corrupt too.
            write_to(&file, &src, &Snapshot { private: false, ..snap }).unwrap();
            assert!(read_from(&file, &src).is_none());
        }
        // An incomplete table (a lookup the volume could not answer) is not
        // kept or offered, and the snapshot still loads.
        let missing = Snapshot {
            root: scan(&src, true, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: false,
            clone_gaps: false,
            private: true,
            private_sizes: true,
            private_missing: true,
            privates: PrivateSizes(std::iter::once((Path::new("d").join("a"), 1)).collect()),
            reflink_fs: None,
        };
        assert!(missing.private_table().is_none());
        write_to(&file, &src, &missing).unwrap();
        let back = read_from(&file, &src).unwrap();
        assert!(back.private_missing && back.privates.0.is_empty() && back.private_table().is_none());
        // Without --private, a snapshot cannot answer a --private request.
        let none = Snapshot {
            root: scan(&src, true, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: None,
        };
        assert!(none.serves(false) && !none.serves(true));
        // Without --clones there are no records to keep: hard links just
        // never count as freeable, and a snapshot claiming otherwise is
        // rejected.
        let plain = Snapshot {
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            ..none
        };
        write_to(&file, &src, &plain).unwrap();
        let again = read_from(&file, &src).unwrap();
        assert!(!again.clones && !answers(&again, true) && answers(&again, false));
        same(&plain.root, &again.root);
        // v8 used the same layout but credited whichever link arrived first.
        // Reject it even when package version and category rules still match.
        let mut legacy = fs::read(&file).unwrap();
        legacy[7] = 8;
        fs::write(&file, legacy).unwrap();
        assert!(read_from(&file, &src).is_none());
        let rebuilt = Snapshot {
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            ..plain
        };
        write_to(&file, &src, &rebuilt).unwrap();
        let normalized = read_from(&file, &src).unwrap();
        same(&rebuilt.root, &normalized.root);
        assert_eq!(
            normalized
                .root
                .children()
                .iter()
                .find(|c| &*c.name == "link")
                .unwrap()
                .size,
            2 << 20
        );
        let plain = rebuilt;
        // The hard-linked file is counted (once) but never freeable.
        assert_eq!(again.root.freeable(), again.root.size - (2 << 20));
        let mislabeled = Snapshot {
            root: scan(&src, true, true, &Progress::default()).unwrap(),
            clones: false,
            ..plain
        };
        write_to(&file, &src, &mislabeled).unwrap();
        assert!(read_from(&file, &src).is_none());
    }

    /// Private sizes survive a round trip only if each names a distinct
    /// file of the tree and is at most its size.
    #[test]
    fn private_sizes_must_name_files() {
        let src = temp("private-table");
        fs::create_dir_all(src.join("d")).unwrap();
        fs::write(src.join("d/a"), vec![3u8; 8192]).unwrap();
        let snap = |entries: &[(&str, u64)]| {
            let mut privates = PrivateSizes::default();
            for &(p, b) in entries {
                privates.0.insert(p.split('/').collect(), b);
            }
            Snapshot {
                root: scan(&src, true, true, &Progress::default()).unwrap(),
                cached_at: None,
                scan_time: Duration::ZERO,
                errors: 0,
                root_dev: None,
                clones: true,
                clone_ids: false,
                clone_gaps: false,
                private: true,
                private_sizes: true,
                private_missing: false,
                privates,
                reflink_fs: None,
            }
        };
        let file = temp("private-table-out").join("c.bin");
        let ok = snap(&[("d/a", 4096)]);
        write_to(&file, &src, &ok).unwrap();
        let back = read_from(&file, &src).unwrap();
        assert_eq!(back.privates, ok.privates);
        assert_eq!(back.private_table().unwrap().get(&Path::new("d").join("a")), Some(4096));
        for bad in [&[("d/a", 1 << 40)][..], &[("d", 1)], &[("d/missing", 1)], &[("a", 1)]] {
            write_to(&file, &src, &snap(bad)).unwrap();
            assert!(read_from(&file, &src).is_none(), "{bad:?}");
        }
        // A backslash is part of a unix file name, not a separator.
        if cfg!(unix) {
            fs::write(src.join("d/x\\y"), vec![3u8; 8192]).unwrap();
            let ok = snap(&[("d/x\\y", 4096)]);
            write_to(&file, &src, &ok).unwrap();
            assert_eq!(read_from(&file, &src).unwrap().privates, ok.privates);
        }
    }

    /// A shared-block record must name a file flagged as sharing blocks:
    /// `account` only visits dirs above such files.
    #[test]
    fn shared_record_on_a_plain_file_is_rejected() {
        let src = temp("shared-bad");
        fs::write(src.join("plain"), vec![1u8; 8192]).unwrap();
        let mut snap = Snapshot {
            root: scan(&src, true, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: None,
        };
        let file = temp("shared-bad-out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        assert!(read_from(&file, &src).is_some());
        let d = snap.root.dir.as_mut().unwrap();
        d.clones_mut().shared = vec![Shared {
            idx: 0,
            key: (1, 2),
            is_clone: true,
            ino: 2,
            nlink: 1,
            refcnt: 2,
            dup: false,
            reflink: None,
        }]
        .into_boxed_slice();
        write_to(&file, &src, &snap).unwrap();
        assert!(read_from(&file, &src).is_none());
    }

    /// Shared extents survive a round trip (needs `DUTYPE_REFLINK_TEST_DIR`,
    /// see `scan::reflink_tests`).
    #[cfg(target_os = "linux")]
    #[test]
    fn round_trip_keeps_shared_extents() {
        use crate::scan::reflink_tests::{fixture, reflink_copy, scan_reflinks};
        let Some(src) = fixture("cache") else { return };
        fs::write(src.join("a"), vec![3u8; 4 << 20]).unwrap();
        fs::File::open(src.join("a")).unwrap().sync_all().unwrap();
        reflink_copy(&src.join("a"), &src.join("sub/b"));
        let (root, reflink_fs) = scan_reflinks(&src);
        assert!(reflink_fs.is_some());
        let snap = Snapshot {
            root,
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs,
        };
        assert_eq!(snap.root.dedup(), snap.root.size - (4 << 20));
        let file = temp("extents-out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        let back = read_from(&file, &src).unwrap();
        same(&snap.root, &back.root);
        assert_eq!((back.clones, back.reflink_fs), (true, reflink_fs));
        assert_eq!(back.root.dedup(), snap.root.dedup());
        fs::remove_dir_all(&src).unwrap();
    }

    /// The reflink filesystem survives a round trip; an unknown one, or one
    /// recorded without `--clones`, is a miss.
    #[test]
    fn reflink_flags_are_checked() {
        let src = temp("flags-src");
        let snap = Snapshot {
            root: scan(&src, true, true, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev: None,
            clones: true,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: Some(ReflinkFs::Xfs),
        };
        let file = temp("flags-out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        let back = read_from(&file, &src).unwrap();
        assert_eq!((back.clones, back.reflink_fs), (true, Some(ReflinkFs::Xfs)));
        let good = fs::read(&file).unwrap();
        // The flags byte follows the magic, fingerprint and three varints;
        // the reflink filesystem byte follows it.
        let mut r = Reader {
            data: &good,
            pos: 16,
            clones: false,
            extents: false,
        };
        (0..3).for_each(|_| assert!(r.varint().is_some()));
        let at = r.pos;
        assert_eq!((good[at], good[at + 1]), (0b100, ReflinkFs::Xfs.code()));
        // An unknown filesystem, or one without the `--clones` flag.
        for (flags, fs) in [(0b100, 4), (0b100, 0xff), (0, ReflinkFs::Xfs.code())] {
            let mut data = good.clone();
            (data[at], data[at + 1]) = (flags, fs);
            fs::write(&file, &data).unwrap();
            assert!(read_from(&file, &src).is_none(), "{flags:#b} {fs}");
        }
    }

    #[test]
    fn cache_dir_inside_root_is_not_counted() {
        let root = fs::canonicalize(temp("excl")).unwrap();
        fs::write(root.join("a.pdf"), vec![1u8; 10_000]).unwrap();
        let cache = root.join(".cache").join("dutype");
        fs::create_dir_all(&cache).unwrap();
        let before = crate::scan::scan_excluding(&root, true, false, &Progress::default(), Some(&cache)).unwrap();
        fs::write(cache.join("snap.bin"), vec![2u8; 200_000]).unwrap();
        let after = crate::scan::scan_excluding(&root, true, false, &Progress::default(), Some(&cache)).unwrap();
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
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors,
            root_dev: None,
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: None,
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
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
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
        };
        write_to(&file, &src, &snap).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
    }

    /// A planted file or symlink at the temp path is never written through,
    /// and our cache directory is made private before writing.
    #[cfg(unix)]
    #[test]
    fn temp_files_are_exclusive_and_dir_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("excl-dir");
        let victim = dir.join("victim.txt");
        fs::write(&victim, "keep").unwrap();
        let link = dir.join("planted.tmp");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(platform::create_private_file(&link).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");

        let src = fs::canonicalize(temp("excl-src")).unwrap();
        let snap = Snapshot {
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
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
        };
        // Snapshots in a directory others can write are not trusted on read.
        fs::write(dir.join("probe"), "x").unwrap();
        fs::set_permissions(dir.join("probe"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(platform::is_private(&dir) && platform::is_private(&dir.join("probe")));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!platform::is_private(&dir));
        fs::set_permissions(dir.join("probe"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!platform::is_private(&dir.join("probe")));

        // A failed rename (the target is a directory) leaves no temp file.
        let blocked = dir.join("blocked.bin");
        fs::create_dir_all(blocked.join("inside")).unwrap();
        let count = || fs::read_dir(&dir).unwrap().count();
        let before = count();
        assert!(write_to(&blocked, &src, &snap).is_err());
        assert_eq!(count(), before);

        // Our own directory open to others is tightened before writing.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        write_to(&dir.join("c.bin"), &src, &snap).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn round_trip() {
        let src = temp("src");
        fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        fs::write(src.join("node_modules/pkg/index.js"), "x".repeat(5000)).unwrap();
        fs::write(src.join("movie.mp4"), vec![0u8; 20_000]).unwrap();
        fs::write(src.join("notes.pdf"), "pdf").unwrap();
        let src = fs::canonicalize(&src).unwrap();
        let root = scan(&src, true, false, &Progress::default()).unwrap();
        let snap = Snapshot {
            root,
            cached_at: None,
            scan_time: Duration::from_millis(1234),
            errors: 3,
            root_dev: Some(42),
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: None,
        };

        let file = temp("out").join("c.bin");
        write_to(&file, &src, &snap).unwrap();
        let back = read_from(&file, &src).unwrap();
        same(&snap.root, &back.root);
        assert_eq!(
            (back.scan_time, back.errors, back.root_dev),
            (snap.scan_time, 3, Some(42))
        );
        assert!(back.cached_at.is_some());

        // Child sizes whose sum overflows u64 are a miss, not a panic or a
        // wrapped total: root dir (size 5) with two files of u64::MAX bytes.
        let mut over = Vec::from(&MAGIC[..]);
        over.extend(reg().fingerprint.to_le_bytes());
        over.extend([0, 0, 0, 0, 0]); // cached_at, scan_time, errors, no root dev
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
        huge.extend([0, 0, 0, 0, 0]);
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
        chain.extend([0, 0, 0, 0, 0]);
        Writer(&mut chain).bytes(src.as_os_str().as_encoded_bytes()).unwrap();
        for _ in 0..400 {
            chain.extend([1, b'd', 0, 0, 1]); // dir "d", size 0
            Writer(&mut chain).varint(4096).unwrap();
        }
        let chain_file = temp("chain").join("c.bin");
        fs::write(&chain_file, &chain).unwrap();
        assert!(read_from(&chain_file, &src).is_none());

        // An overlong varint (10th byte > 1) is rejected, not truncated.
        let mut r = Reader {
            data: &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02],
            pos: 0,
            clones: false,
            extents: false,
        };
        assert_eq!(r.varint(), None);
        let mut r = Reader {
            data: &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            pos: 0,
            clones: false,
            extents: false,
        };
        assert_eq!(r.varint(), Some(u64::MAX));

        // A corrupt timestamp is a miss, not a panic.
        let mut bad = Vec::from(&MAGIC[..]);
        bad.extend(reg().fingerprint.to_le_bytes());
        bad.extend([0xff; 9]);
        bad.push(0x01); // u64::MAX seconds, beyond SystemTime on some platforms
        let bad_file = temp("bad").join("c.bin");
        fs::write(&bad_file, &bad).unwrap();
        assert!(read_from(&bad_file, &src).is_none());

        // A snapshot from another device (root remounted since) is stale.
        let dev = crate::scan::device_of(&src).unwrap();
        let on = |root_dev| Snapshot {
            root: scan(&src, true, false, &Progress::default()).unwrap(),
            cached_at: None,
            scan_time: Duration::ZERO,
            errors: 0,
            root_dev,
            clones: false,
            clone_ids: false,
            clone_gaps: false,
            private: false,
            private_sizes: false,
            private_missing: false,
            privates: PrivateSizes::default(),
            reflink_fs: None,
        };
        assert!(same_device(&on(Some(dev)), &src));
        assert!(!same_device(&on(Some(dev.wrapping_add(1))), &src));

        // Another path, or a truncated file, is a miss rather than a wrong tree.
        assert!(read_from(&file, &src.join("node_modules")).is_none());
        let data = fs::read(&file).unwrap();
        fs::write(&file, &data[..data.len() - 1]).unwrap();
        assert!(read_from(&file, &src).is_none());
    }
}
