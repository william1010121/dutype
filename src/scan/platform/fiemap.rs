//! `--clones` on Linux: which physical ranges of a file are shared with
//! other files (reflinked copies, deduplicated extents, btrfs snapshots),
//! from the `FS_IOC_FIEMAP` ioctl. That costs syscalls per file, so it is
//! opt-in and runs in a parallel pass after the scan
//! (`scan::read_shared_extents`), never in the directory walk itself.
//!
//! This is the Linux side of issue #5's shared-extent probe: APFS names
//! full-clone families in the bulk directory read (`Entry::clone`), reflink
//! filesystems only tell which physical ranges a file shares, and
//! [`crate::account`] counts each shared range once.

use crate::scan::Extents;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

// From linux/fiemap.h and linux/fs.h.
const FS_IOC_FIEMAP: u64 = 0xC020_660B; // _IOWR('f', 11, struct fiemap)
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
const FIEMAP_EXTENT_ENCODED: u32 = 0x8;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
/// No usable physical location (`btrfs filesystem du` skips these).
const SKIP: u32 = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_DATA_INLINE;

/// Extents fetched per ioctl.
const BATCH: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [FiemapExtent; BATCH],
}

/// What FIEMAP tells about one file's shared extents.
#[derive(Default)]
pub struct Probed {
    /// Physical ranges `(start, len)` of the shared extents, sorted,
    /// adjacent ranges merged; `None` when nothing is shared.
    pub extents: Option<Extents>,
    /// The file also shares extents whose physical size is unknown: encoded
    /// (compressed) extents report their logical length, so they are left
    /// out of `extents`.
    pub opaque: bool,
    /// `(st_dev, st_ino)` of the file.
    pub id: (u64, u64),
}

/// Reads the shared extents of files, reusing one FIEMAP buffer.
pub struct SharedExtentProbe {
    map: Box<Fiemap>,
}

impl SharedExtentProbe {
    pub fn new() -> Self {
        SharedExtentProbe {
            map: Box::new(Fiemap {
                start: 0,
                length: 0,
                flags: 0,
                mapped_extents: 0,
                extent_count: 0,
                reserved: 0,
                extents: [FiemapExtent::default(); BATCH],
            }),
        }
    }

    /// The shared extents of the file at `path`; nothing for one that is
    /// not a regular file (a symlink, socket or device). An error means it
    /// could not be read.
    /// Stops between ioctls once `cancel` is set (the result is then
    /// incomplete and meant to be thrown away).
    pub fn probe(&mut self, path: &Path, cancel: &AtomicBool) -> io::Result<Probed> {
        let fd = match open(path) {
            Ok(fd) => fd,
            Err(e) => return open_failed(e),
        };
        let file = std::fs::File::from(fd);
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Ok(Probed::default());
        }
        let mut ranges = Vec::new();
        let mut opaque = false;
        let mut start = 0u64;
        loop {
            // A file with many extents takes many batches.
            if cancel.load(Ordering::Relaxed) {
                return Ok(Probed::default());
            }
            let m = &mut *self.map;
            m.start = start;
            m.length = u64::MAX - start;
            m.flags = 0;
            m.mapped_extents = 0;
            m.extent_count = BATCH as u32;
            m.reserved = 0;
            // SAFETY: `m` is a `struct fiemap` with room for `extent_count`
            // extents, as FS_IOC_FIEMAP expects.
            if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, m as *mut Fiemap) } < 0 {
                return fiemap_failed(io::Error::last_os_error());
            }
            let n = (m.mapped_extents as usize).min(BATCH);
            let Some(last) = n.checked_sub(1).map(|i| m.extents[i]) else {
                break;
            };
            for x in m.extents[..n].iter() {
                match classify(x.flags, x.length) {
                    Extent::Private => {}
                    Extent::Shared => ranges.push((x.physical, x.length)),
                    Extent::Opaque => opaque = true,
                }
            }
            let next = last.logical.saturating_add(last.length);
            if last.flags & FIEMAP_EXTENT_LAST != 0 || next <= start {
                break;
            }
            start = next;
        }
        Ok(Probed {
            extents: merge(ranges),
            opaque,
            id: (meta.dev(), meta.ino()),
        })
    }
}

/// What an extent means for shared-extent accounting.
#[derive(Debug, PartialEq, Eq)]
enum Extent {
    /// Not shared (or empty).
    Private,
    /// Shared, at a known physical range.
    Shared,
    /// Shared, but where its bytes are is unknown: encoded (compressed:
    /// the length is logical), or without a usable location (unknown,
    /// delayed allocation, inline data).
    Opaque,
}

fn classify(flags: u32, length: u64) -> Extent {
    if flags & FIEMAP_EXTENT_SHARED == 0 || length == 0 {
        Extent::Private
    } else if flags & (SKIP | FIEMAP_EXTENT_ENCODED) != 0 {
        Extent::Opaque
    } else {
        Extent::Shared
    }
}

/// A file that could not be opened: a symlink (`O_NOFOLLOW`) or a socket
/// has no data extents to share; anything else could not be probed.
fn open_failed(e: io::Error) -> io::Result<Probed> {
    match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENXIO) => Ok(Probed::default()),
        _ => Err(e),
    }
}

/// FIEMAP failed on a regular file with blocks. Even "not supported"
/// (ENOTTY, EOPNOTSUPP, EINVAL) means its extents are unknown: a gap in the
/// detection, never a file without sharing.
fn fiemap_failed(e: io::Error) -> io::Result<Probed> {
    Err(e)
}

/// Sorted by start, with ranges that touch end to start merged (overlapping
/// ones are kept: the file holds those bytes twice).
fn merge(mut ranges: Vec<(u64, u64)>) -> Option<Extents> {
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, len) in ranges {
        match out.last_mut() {
            Some((s, l)) if s.checked_add(*l) == Some(start) => *l += len,
            _ => out.push((start, len)),
        }
    }
    Some(out.into_boxed_slice())
}

/// `open(path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_NOCTTY | O_CLOEXEC)`.
fn open(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: `c` is NUL-terminated; open has no other preconditions.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_extents_without_a_location_are_opaque() {
        const M: u64 = 1 << 20;
        assert_eq!(classify(0, M), Extent::Private);
        assert_eq!(
            classify(FIEMAP_EXTENT_LAST | FIEMAP_EXTENT_DELALLOC, M),
            Extent::Private
        );
        assert_eq!(classify(FIEMAP_EXTENT_SHARED, M), Extent::Shared);
        assert_eq!(classify(FIEMAP_EXTENT_SHARED | FIEMAP_EXTENT_LAST, M), Extent::Shared);
        assert_eq!(classify(FIEMAP_EXTENT_SHARED, 0), Extent::Private);
        for f in [
            FIEMAP_EXTENT_ENCODED,
            FIEMAP_EXTENT_UNKNOWN,
            FIEMAP_EXTENT_DELALLOC,
            FIEMAP_EXTENT_DATA_INLINE,
        ] {
            assert_eq!(classify(FIEMAP_EXTENT_SHARED | f, M), Extent::Opaque, "{f:#x}");
        }
    }

    /// Cancelled before (or between) FIEMAP batches: nothing is read.
    #[test]
    fn probe_stops_when_cancelled() {
        let d = std::env::temp_dir().join(format!("dutype-fiemap-cancel-{}", std::process::id()));
        std::fs::write(&d, vec![1u8; 1 << 16]).unwrap();
        std::fs::File::open(&d).unwrap().sync_all().unwrap();
        let mut p = SharedExtentProbe::new();
        let cancel = AtomicBool::new(true);
        let got = p.probe(&d, &cancel).unwrap();
        assert!(got.extents.is_none() && !got.opaque && got.id == (0, 0));
        // Not cancelled: the file is read (its id is known).
        match p.probe(&d, &AtomicBool::new(false)) {
            Ok(got) => assert_ne!(got.id, (0, 0)),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTTY | libc::EOPNOTSUPP)) => {
                // Cancellation above is still tested on filesystems without
                // FIEMAP (e.g. tmpfs). Other failures must not be hidden.
                eprintln!("FIEMAP unsupported on this test filesystem: {e}");
            }
            Err(e) => panic!("unexpected FIEMAP failure: {e}"),
        }
        std::fs::remove_file(&d).unwrap();
    }
    use std::io;

    #[test]
    fn only_non_files_are_not_gaps() {
        let os = io::Error::from_raw_os_error;
        for errno in [libc::ELOOP, libc::ENXIO] {
            assert!(open_failed(os(errno)).is_ok_and(|p| p.extents.is_none() && !p.opaque));
        }
        for errno in [libc::EACCES, libc::EPERM, libc::ENOENT, libc::EMFILE] {
            assert!(open_failed(os(errno)).is_err(), "{errno}");
        }
        for errno in [libc::ENOTTY, libc::EOPNOTSUPP, libc::EINVAL, libc::EIO] {
            assert!(fiemap_failed(os(errno)).is_err(), "{errno}");
        }
    }

    #[test]
    fn merges_adjacent_keeps_overlaps() {
        assert_eq!(merge(vec![]), None);
        assert_eq!(
            merge(vec![(8192, 4096), (0, 4096), (4096, 4096), (100, 10), (100, 10)]).as_deref(),
            Some(&[(0, 4096), (100, 10), (100, 10), (4096, 8192)][..])
        );
    }
}
