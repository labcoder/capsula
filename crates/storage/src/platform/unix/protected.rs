use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use crate::platform::Untrusted;

pub(crate) const DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

pub(crate) fn components(path: &Path) -> Result<Vec<CString>, Untrusted> {
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

pub(crate) fn open_directory(parent: RawFd, name: &CStr) -> Result<File, Untrusted> {
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

pub(crate) fn stat(fd: RawFd) -> Result<libc::stat, Untrusted> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: metadata has the libc stat layout and writable storage; fd is not consumed.
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
        return Err(Untrusted::Io);
    }
    // SAFETY: successful fstat initialized the complete stat value.
    Ok(unsafe { metadata.assume_init() })
}

pub(crate) fn safe_directory(metadata: &libc::stat, user: libc::uid_t) -> Result<(), Untrusted> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR
        || (metadata.st_uid != 0 && metadata.st_uid != user)
        || metadata.st_mode & (libc::S_IWGRP | libc::S_IWOTH) != 0
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

pub(crate) fn same_inspection(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_uid == right.st_uid
        && left.st_gid == right.st_gid
        && left.st_mode == right.st_mode
}

pub(crate) fn safe_regular(metadata: &libc::stat, user: libc::uid_t) -> Result<(), Untrusted> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != user
        || metadata.st_mode & 0o7777 != 0o600
        || metadata.st_nlink != 1
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}
