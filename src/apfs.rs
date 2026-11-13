//! Thin wrappers around the macOS APIs used to find and create APFS clones.
//!
//! Constants that the `libc` crate does not expose are defined here with a
//! reference to the SDK header they come from.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `<sys/clonefile.h>`: don't follow a symlink at the source path.
const CLONE_NOFOLLOW: u32 = 0x0001;
/// `<sys/clonefile.h>`: don't copy ownership from the source.
const CLONE_NOOWNERCOPY: u32 = 0x0002;
/// `<sys/stat.h>`: the file shares all of its blocks with another file.
const EF_SHARES_ALL_BLOCKS: u64 = 0x0000_0040;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Identifies the data blocks backing a file.
///
/// Two files with the same `Shared` value are full clones of each other and
/// deduplicating them again would save nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BlockSet {
    Shared { dev: u64, clone_id: u64 },
    Private { dev: u64, ino: u64 },
}

/// Returns which block set `path` belongs to.
///
/// Files that only share some of their blocks, or whose clone information is
/// unavailable, are treated as private.
pub fn block_set(path: &Path, dev: u64, ino: u64) -> BlockSet {
    match clone_info(path) {
        Some((clone_id, ext_flags)) if ext_flags & EF_SHARES_ALL_BLOCKS != 0 => {
            BlockSet::Shared { dev, clone_id }
        }
        _ => BlockSet::Private { dev, ino },
    }
}

/// Returns `(clone id, extended flags)` from `getattrlist(2)`.
fn clone_info(path: &Path) -> Option<(u64, u64)> {
    const ATTRS: libc::attrgroup_t = libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS;

    // Attributes are packed in bit order, each 4-byte aligned. `CLONEID`
    // (0x100) comes before `EXT_FLAGS` (0x200).
    #[repr(C, packed(4))]
    struct AttrBuf {
        length: u32,
        returned: libc::attribute_set_t,
        clone_id: u64,
        ext_flags: u64,
    }

    let path = c_path(path).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTRS,
    };
    // SAFETY: `AttrBuf` only contains integers, for which all-zero is valid.
    let mut buf: AttrBuf = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `buf` is valid for writes of its size.
    let rc = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut list).cast(),
            (&raw mut buf).cast(),
            size_of::<AttrBuf>(),
            (libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_PACK_INVAL_ATTRS)
                as _,
        )
    };
    if rc != 0 {
        return None;
    }
    let returned = buf.returned.forkattr;
    if returned & ATTRS != ATTRS {
        return None;
    }
    Some((buf.clone_id, buf.ext_flags))
}

/// Creates `dst` as an APFS clone of `src` with `clonefile(2)`.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (c_path(src)?, c_path(dst)?);
    // SAFETY: both paths are NUL-terminated.
    check(unsafe {
        libc::clonefile(
            src.as_ptr(),
            dst.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    })
}

/// Copies mode, flags, ACLs and extended attributes of `from` onto `to`
/// with `copyfile(3)`.
pub fn copy_metadata(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated; a null state is allowed.
    let rc = unsafe {
        libc::copyfile(
            from.as_ptr(),
            to.as_ptr(),
            std::ptr::null_mut(),
            libc::COPYFILE_METADATA | libc::COPYFILE_NOFOLLOW,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Sets access and modification times of `path` (without following symlinks).
pub fn set_times(path: &Path, atime: (i64, i64), mtime: (i64, i64)) -> io::Result<()> {
    let path = c_path(path)?;
    let times = [
        libc::timespec {
            tv_sec: atime.0,
            tv_nsec: atime.1,
        },
        libc::timespec {
            tv_sec: mtime.0,
            tv_nsec: mtime.1,
        },
    ];
    // SAFETY: `path` is NUL-terminated and `times` has the two required entries.
    check(unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
}

/// Returns whether another process holds a `flock(2)` or `fcntl(2)` lock on
/// `path`. Unreadable files are reported as locked.
pub fn is_locked(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return true;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
        }
        Err(_) => return true,
    }
    // SAFETY: `flock` only contains integers, for which all-zero is valid.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    // SAFETY: `file` is open and `lock` is a valid `struct flock`.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    rc != 0 || lock.l_type != libc::F_UNLCK as _
}

/// Returns the mount point of the volume holding `path` and the bytes
/// available on it to unprivileged users.
pub fn volume_space(path: &Path) -> Option<(PathBuf, u64)> {
    let path = c_path(path).ok()?;
    // SAFETY: `statfs` only contains integers and arrays of them.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `st` is valid for writes.
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // SAFETY: the kernel NUL-terminates `f_mntonname`.
    let mount = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
    let mount = PathBuf::from(OsStr::from_bytes(mount.to_bytes()));
    Some((mount, st.f_bavail * u64::from(st.f_bsize)))
}
