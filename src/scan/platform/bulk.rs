//! macOS directory reader built on `getattrlistbulk(2)`: one syscall returns
//! the names and metadata of many entries, instead of `readdir` plus one
//! `lstat` per entry. Sizes and identities match what `lstat` reports.

use crate::scan::{Entry, ReadStats};
use std::cell::RefCell;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

// Missing from libc 0.2 (values from the SDK's sys/attr.h and sys/vnode.h).
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x1000;
const EF_MAY_SHARE_BLOCKS: u64 = 0x1;
const EF_SHARES_ALL_BLOCKS: u64 = 0x40;
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;
const VDIR: u32 = 2;

/// Bytes per `getattrlistbulk` call; kept per thread and reused.
const BUF_WORDS: usize = 128 * 1024 / 8;

thread_local! {
    // u64 words: the kernel aligns each entry to 8 bytes in the buffer.
    static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0; BUF_WORDS]);
}

/// What `getattrlistbulk` returns; with `clones`, also the APFS clone
/// identity (otherwise exactly the plain scan's request, at its speed).
fn attr_list(clones: bool) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        // OBJTYPE must be requested, or xnu falls back to a slow generic
        // implementation instead of the filesystem's native one.
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS | libc::ATTR_DIR_ALLOCSIZE,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        // With FSOPT_ATTR_CMN_EXTENDED, forkattr holds the extended common
        // attributes. Not ATTR_CMNEXT_CLONE_REFCNT: APFS serializes that
        // lookup across threads, which made parallel scans 2x slower; it is
        // read per family instead (see `clone_refcnt`).
        forkattr: match clones {
            true => libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS,
            false => 0,
        },
    }
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Like [`crate::scan::read_entries_std`]. `None` when the volume does not support
/// bulk reads (the caller then falls back to `read_dir`).
pub(in crate::scan) fn read_entries(
    path: &Path,
    cancel: &AtomicBool,
    out: &mut Vec<Entry>,
    clones: bool,
) -> Option<io::Result<ReadStats>> {
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return Some(Err(io::ErrorKind::InvalidInput.into()));
    };
    // Follows a symlink only for the scan root (children are known dirs).
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Some(Err(io::Error::last_os_error()));
    }
    let fd = Fd(fd);
    BUF.with_borrow_mut(|buf| read_all(&fd, buf, cancel, out, clones))
}

fn read_all(
    fd: &Fd,
    buf: &mut [u64],
    cancel: &AtomicBool,
    out: &mut Vec<Entry>,
    clones: bool,
) -> Option<io::Result<ReadStats>> {
    let mut list = attr_list(clones);
    let mut stats = ReadStats::default();
    let mut first = true;
    loop {
        let bytes = std::mem::size_of_val(buf);
        let options = if clones {
            libc::FSOPT_ATTR_CMN_EXTENDED as u64
        } else {
            0
        };
        let n = unsafe { libc::getattrlistbulk(fd.0, (&raw mut list).cast(), buf.as_mut_ptr().cast(), bytes, options) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if first && matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
                return None;
            }
            // Like a failing `read_dir` iteration: what was read is kept.
            return Some(match first {
                true => Err(err),
                false => {
                    stats.errors += 1;
                    Ok(stats)
                }
            });
        }
        if n == 0 {
            return Some(Ok(stats));
        }
        first = false;
        let data = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes) };
        let mut at = 0;
        for _ in 0..n {
            let Some(len) = read_u32(data, at)
                .map(|l| l as usize)
                .filter(|&l| l >= 4 && at + l <= data.len())
            else {
                // A malformed buffer: drop the rest of the directory.
                stats.errors += 1;
                return Some(Ok(stats));
            };
            match parse(fd, &data[at..at + len], clones, &mut stats.clone_ids) {
                Some(e) => out.push(e),
                None => stats.errors += 1,
            }
            at += len;
        }
        if cancel.load(Ordering::Relaxed) {
            return Some(Ok(stats));
        }
    }
}

fn read_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// Reads fields in order; every field is 4-byte aligned, 64-bit ones too.
struct Cursor<'a> {
    d: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn u32(&mut self) -> Option<u32> {
        let v = read_u32(self.d, self.at)?;
        self.at += 4;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let v = read_u64(self.d, self.at)?;
        self.at += 8;
        Some(v)
    }
}

/// One entry, laid out as described in getattrlist(2): length, returned
/// attribute set, then each returned attribute in bit order, group by group
/// (common, dir, file, extended common). `None` for an entry with no
/// usable name or metadata. Sets `clone_ids` when clone flags are reported.
fn parse(fd: &Fd, d: &[u8], clones: bool, clone_ids: &mut bool) -> Option<Entry> {
    let mut c = Cursor { d, at: 4 };
    let common = c.u32()?;
    let _vol = c.u32()?;
    let dir_attrs = c.u32()?;
    let file_attrs = c.u32()?;
    let ext_attrs = c.u32()?;
    // ATTR_CMN_ERROR comes right after the returned set.
    let error = match common & ATTR_CMN_ERROR != 0 {
        true => c.u32()?,
        false => 0,
    };
    if common & libc::ATTR_CMN_NAME == 0 {
        return None;
    }
    let name_at = c.at;
    let off = c.u32()? as i32;
    let len = c.u32()? as usize;
    let start = name_at.checked_add_signed(off as isize)?;
    // The length includes the terminating NUL.
    let raw = d.get(start..start.checked_add(len)?)?;
    let name = OsStr::from_bytes(raw.strip_suffix(&[0]).unwrap_or(raw));
    // An entry the kernel could not read in bulk may still `lstat`.
    let need = libc::ATTR_CMN_DEVID | libc::ATTR_CMN_OBJTYPE | libc::ATTR_CMN_FILEID;
    if error != 0 || common & need != need {
        return lstat_at(fd, name, clones);
    }
    // dev_t is signed; widen it like `MetadataExt::dev` does.
    let dev = c.u32()? as i32 as u64;
    let objtype = c.u32()?;
    let ino = c.u64()?;
    if objtype == VDIR {
        let mount = match dir_attrs & libc::ATTR_DIR_MOUNTSTATUS != 0 {
            true => c.u32()?,
            false => 0,
        };
        let alloc = match dir_attrs & libc::ATTR_DIR_ALLOCSIZE != 0 {
            true => Some(c.u64()?),
            false => None,
        };
        // A mount point reports the attributes of the filesystem below it;
        // `lstat` gives the mounted one, as a `read_dir` scan sees it.
        if mount & (libc::DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0 || alloc.is_none() {
            return lstat_at(fd, name, clones);
        }
        return Some(Entry {
            name: name.to_os_string(),
            dir: true,
            alloc: alloc?,
            dev,
            ino,
            nlink: 1,
            clone: None,
            may_share: false,
        });
    }
    let nlink = match file_attrs & libc::ATTR_FILE_LINKCOUNT != 0 {
        true => c.u32()?,
        false => return lstat_at(fd, name, clones),
    };
    let alloc = match file_attrs & libc::ATTR_FILE_ALLOCSIZE != 0 {
        true => c.u64()?,
        false => return lstat_at(fd, name, clones),
    };
    let clone_id = match ext_attrs & libc::ATTR_CMNEXT_CLONEID != 0 {
        true => Some(c.u64()?),
        false => None,
    };
    let flags = match ext_attrs & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
        true => c.u64()?,
        false => 0,
    };
    if ext_attrs & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
        *clone_ids = true;
    }
    Some(Entry {
        name: name.to_os_string(),
        dir: false,
        alloc,
        dev,
        ino,
        nlink: nlink as u64,
        // Only a family with other members matters: a file that shares all
        // its blocks has at least one identical clone.
        clone: clone_id.filter(|_| flags & EF_SHARES_ALL_BLOCKS != 0),
        may_share: flags & EF_MAY_SHARE_BLOCKS != 0,
    })
}

/// Fallback for an entry whose bulk attributes are incomplete or describe
/// another filesystem.
/// With `clones`, a file read this way has unknown clone flags, so it
/// counts as one that may share blocks (never as freeable).
fn lstat_at(fd: &Fd, name: &OsStr, clones: bool) -> Option<Entry> {
    let cname = CString::new(name.as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstatat(fd.0, cname.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    let dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    Some(Entry {
        name: name.to_os_string(),
        dir,
        alloc: st.st_blocks as u64 * 512,
        dev: st.st_dev as u64,
        ino: st.st_ino,
        nlink: if dir { 1 } else { st.st_nlink as u64 },
        clone: None,
        may_share: clones && !dir,
    })
}

/// Number of files in the APFS full-clone family of the file at `path`
/// (macOS 14+). `None` when the volume or the system cannot tell.
pub(in crate::scan) fn clone_refcnt(path: &Path) -> Option<u32> {
    // Sealed system volumes report 0.
    ext_attr::<u32>(path, ATTR_CMNEXT_CLONE_REFCNT).filter(|&n| n > 0)
}

/// The file at `path` shares, or may share, blocks with another file.
pub(in crate::scan) fn may_share(path: &Path) -> bool {
    ext_attr::<u64>(path, libc::ATTR_CMNEXT_EXT_FLAGS)
        .is_some_and(|f| f & (EF_MAY_SHARE_BLOCKS | EF_SHARES_ALL_BLOCKS) != 0)
}

/// One extended common attribute of `path` (not following a symlink).
fn ext_attr<T: Copy>(path: &Path, attr: u32) -> Option<T> {
    #[repr(C, packed(4))]
    #[derive(Clone, Copy)]
    struct Reply<T: Copy> {
        len: u32,
        returned: libc::attribute_set_t,
        value: T,
    }
    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: attr,
    };
    let mut reply = std::mem::MaybeUninit::<Reply<T>>::zeroed();
    let options = libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW;
    let size = std::mem::size_of::<Reply<T>>();
    let rc = unsafe {
        libc::getattrlist(
            cpath.as_ptr(),
            (&raw mut list).cast(),
            reply.as_mut_ptr().cast(),
            size,
            options,
        )
    };
    if rc != 0 {
        return None;
    }
    let reply = unsafe { reply.assume_init() };
    let returned = reply.returned;
    (returned.forkattr & attr != 0).then_some(reply.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding `f.bin` and a synthetic bulk entry for it with
    /// the given common attributes (and `error` if ATTR_CMN_ERROR is set):
    /// length, returned set, then the name's attrreference pointing just
    /// past itself. Later attributes are left out.
    fn synthetic(tag: &str, common: u32, error: u32) -> (std::path::PathBuf, Fd, Vec<u8>) {
        let d = std::env::temp_dir().join(format!("dutype-bulk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.bin"), vec![1u8; 10_000]).unwrap();
        let cpath = CString::new(d.as_os_str().as_bytes()).unwrap();
        let fd = Fd(unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) });
        assert!(fd.0 >= 0);
        let name = b"f.bin\0";
        let mut e = Vec::new();
        for w in [
            0,
            common,
            0,
            0,
            libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
            0,
        ] {
            e.extend_from_slice(&w.to_ne_bytes());
        }
        if common & ATTR_CMN_ERROR != 0 {
            e.extend_from_slice(&error.to_ne_bytes());
        }
        e.extend_from_slice(&8i32.to_ne_bytes());
        e.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        e.extend_from_slice(name);
        let len = e.len() as u32;
        e[..4].copy_from_slice(&len.to_ne_bytes());
        (d, fd, e)
    }

    /// With --clones, an entry read through the lstat fallback has unknown
    /// clone flags: it may share blocks, so it is never counted as freeable.
    #[test]
    fn fallback_entry_may_share_with_clones() {
        let common = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | ATTR_CMN_ERROR;
        let (d, fd, e) = synthetic("fallback-clones", common, libc::EIO as u32);
        let got = parse(&fd, &e, true, &mut false).expect("entry kept");
        assert!(got.may_share && got.clone.is_none());
        assert!(!parse(&fd, &e, false, &mut false).unwrap().may_share);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn plain_request_has_no_extended_attributes() {
        assert_eq!(attr_list(false).forkattr, 0);
        assert_ne!(attr_list(true).forkattr, 0);
    }

    #[test]
    fn missing_common_attrs_fall_back_to_lstat() {
        let common =
            libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_DEVID | libc::ATTR_CMN_OBJTYPE;
        let (d, fd, e) = synthetic("attrs", common, 0);
        let got = parse(&fd, &e, false, &mut false).expect("entry kept");
        assert_eq!(Some(got), lstat_at(&fd, OsStr::new("f.bin"), false));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn entry_error_falls_back_to_lstat() {
        let common = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | ATTR_CMN_ERROR;
        let (d, fd, e) = synthetic("error", common, libc::EIO as u32);
        let got = parse(&fd, &e, false, &mut false).expect("entry kept");
        assert_eq!(Some(got), lstat_at(&fd, OsStr::new("f.bin"), false));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
