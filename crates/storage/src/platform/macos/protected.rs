use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::NonNull;

use crate::platform::Untrusted;

const DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

// SDK sys/fcntl.h's filesec_property_t; libc does not declare the filesec APIs.
const FILESEC_OWNER: libc::c_int = 1;
const FILESEC_MODE: libc::c_int = 4;
const FILESEC_ACL: libc::c_int = 5;

unsafe extern "C" {
    fn filesec_init() -> *mut libc::c_void;
    fn filesec_free(filesec: *mut libc::c_void);
    fn filesec_get_property(
        filesec: *mut libc::c_void,
        property: libc::c_int,
        output: *mut libc::c_void,
    ) -> libc::c_int;
    fn filesec_query_property(
        filesec: *mut libc::c_void,
        property: libc::c_int,
        present: *mut libc::c_int,
    ) -> libc::c_int;
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(
        fd: libc::c_int,
        metadata: *mut libc::stat,
        filesec: *mut libc::c_void,
    ) -> libc::c_int;
}

/// Opens and inspects an absolute directory chain, retaining each object from root to leaf.
///
/// Fds do not freeze names. This admission alone cannot authorize native loading or launch,
/// and does not establish real application anchors or Windows integrity-label equivalence.
///
/// # Errors
/// Refuses non-absolute, dot, parent or NUL components, links, device crossings, unsafe
/// ownership/modes, any present ACL, and every incomplete or failed inspection.
pub fn hold_chain(path: &Path) -> Result<Vec<File>, Untrusted> {
    if !path.is_absolute() {
        return Err(Untrusted::Unsafe);
    }
    let names = components(path)?;
    // SAFETY: getuid has no arguments or ownership effects.
    let user = unsafe { libc::getuid() };
    let root = open_directory(libc::AT_FDCWD, c"/")?;
    hold_from(root, &names, user)
}

fn components(path: &Path) -> Result<Vec<CString>, Untrusted> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.starts_with(b"//") {
        return Err(Untrusted::Unsafe);
    }
    bytes
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
        .map(|name| {
            if name == b"." || name == b".." {
                return Err(Untrusted::Unsafe);
            }
            CString::new(name).map_err(|_| Untrusted::Unsafe)
        })
        .collect()
}

fn hold_from(root: File, names: &[CString], user: libc::uid_t) -> Result<Vec<File>, Untrusted> {
    let device = inspect(&root, user)?.st_dev;
    let mut held = vec![root];
    for name in names {
        let parent = held.last().ok_or(Untrusted::Io)?;
        let child = open_directory(parent.as_raw_fd(), name)?;
        let metadata = inspect(&child, user)?;
        // shortcut: refuse device crossings; real APFS/firmlink anchor admission needs its own gate.
        if metadata.st_dev != device {
            return Err(Untrusted::Unsafe);
        }
        held.push(child);
    }
    Ok(held)
}

fn open_directory(parent: RawFd, name: &CStr) -> Result<File, Untrusted> {
    // SAFETY: name is terminated; parent is held (or AT_FDCWD for the root); no creation flag.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), DIRECTORY_FLAGS) };
    if fd < 0 {
        return Err(match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Untrusted::Missing,
            Some(libc::ELOOP) => Untrusted::Reparse,
            Some(libc::ENOTDIR) => Untrusted::Unsafe,
            _ => Untrusted::Io,
        });
    }
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat(fd: RawFd) -> Result<libc::stat, Untrusted> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: metadata has the libc stat layout and writable storage; fd is not consumed.
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
        return Err(Untrusted::Io);
    }
    // SAFETY: successful fstat initialized the complete stat value.
    Ok(unsafe { metadata.assume_init() })
}

fn safe_directory(metadata: &libc::stat, user: libc::uid_t) -> Result<(), Untrusted> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR
        || (metadata.st_uid != 0 && metadata.st_uid != user)
        || metadata.st_mode & (libc::S_IWGRP | libc::S_IWOTH) != 0
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

fn same_inspection(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_uid == right.st_uid
        && left.st_gid == right.st_gid
        && left.st_mode == right.st_mode
}

fn inspect(file: &File, user: libc::uid_t) -> Result<libc::stat, Untrusted> {
    let before = stat(file.as_raw_fd())?;
    safe_directory(&before, user)?;
    let security = Filesec::new()?;
    security.inspect(file.as_raw_fd(), &before)?;
    let after = stat(file.as_raw_fd())?;
    if !same_inspection(&before, &after) {
        return Err(Untrusted::Io);
    }
    Ok(after)
}

struct Filesec(NonNull<libc::c_void>);

impl Filesec {
    fn new() -> Result<Self, Untrusted> {
        // SAFETY: filesec_init has no arguments and returns a newly owned filesec or null.
        NonNull::new(unsafe { filesec_init() })
            .map(Self)
            .ok_or(Untrusted::Io)
    }

    fn inspect(&self, fd: RawFd, expected: &libc::stat) -> Result<(), Untrusted> {
        // SAFETY: all-zero libc stat is valid initialized storage, not an inspection result.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is borrowed; metadata is writable libc stat and self owns a fresh filesec.
        if unsafe { fstatx_np(fd, &raw mut metadata, self.0.as_ptr()) } != 0 {
            return Err(Untrusted::Io);
        }
        if !same_inspection(expected, &metadata) {
            return Err(Untrusted::Io);
        }
        let mut owner = libc::uid_t::MAX;
        // SAFETY: FILESEC_OWNER writes a uid_t; the filesec and output are live.
        if unsafe { filesec_get_property(self.0.as_ptr(), FILESEC_OWNER, (&raw mut owner).cast()) }
            != 0
            || owner != expected.st_uid
        {
            return Err(Untrusted::Io);
        }
        let mut mode = libc::mode_t::MAX;
        // SAFETY: FILESEC_MODE writes a mode_t; the filesec and output are live.
        if unsafe { filesec_get_property(self.0.as_ptr(), FILESEC_MODE, (&raw mut mode).cast()) }
            != 0
            || mode != expected.st_mode
        {
            return Err(Untrusted::Io);
        }
        let mut present = -1;
        // SAFETY: FILESEC_ACL queries presence into a live int without allocating an ACL.
        let result =
            unsafe { filesec_query_property(self.0.as_ptr(), FILESEC_ACL, &raw mut present) };
        acl_absent(result, present)
    }
}

fn acl_absent(result: libc::c_int, present: libc::c_int) -> Result<(), Untrusted> {
    if result != 0 || present < 0 {
        return Err(Untrusted::Io);
    }
    if present != 0 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

impl Drop for Filesec {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner; filesec_free is void and is called exactly once.
        unsafe { filesec_free(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests;
