use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};

use super::protected;
use crate::Untrusted;

/// The acquisition stage that failed, without paths or foreign error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AcquisitionError {
    /// Ordinary creation/open failure, not a completed security inspection.
    #[error("private object creation or open failed")]
    Open(io::ErrorKind),
    /// Privacy, type, volume, ACL or identity was unsafe or could not be established.
    #[error("private object admission failed: {0}")]
    Admission(#[from] Untrusted),
}

pub(crate) fn private_directory(metadata: &libc::stat, user: libc::uid_t) -> Result<(), Untrusted> {
    if metadata.st_uid != user || metadata.st_mode & 0o7777 != 0o700 {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

pub(crate) fn basename(name: &str) -> Result<CString, Untrusted> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(Untrusted::Unsafe);
    }
    CString::new(name).map_err(|_| Untrusted::Unsafe)
}

pub(crate) fn open_at(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, AcquisitionError> {
    // SAFETY: name is terminated, parent is retained or AT_FDCWD for root; mode is promoted for C.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP) => Untrusted::Reparse.into(),
            Some(libc::ENOTDIR | libc::EISDIR) => Untrusted::Unsafe.into(),
            _ => AcquisitionError::Open(error.kind()),
        });
    }
    // SAFETY: successful openat transferred this new uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn entry_stat(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: retained parent, terminated basename and writable stat storage; no link following.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

pub(crate) fn named(parent: &File, name: &CStr, expected: &libc::stat) -> Result<(), Untrusted> {
    let actual = entry_stat(parent, name).map_err(|_| Untrusted::Io)?;
    // A folder's link count moves whenever anyone adds or removes a subfolder in it; only a
    // file's says whether it gained another name.
    if !protected::same_inspection(&actual, expected)
        || (expected.st_mode & libc::S_IFMT == libc::S_IFREG
            && actual.st_nlink != expected.st_nlink)
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}

pub(crate) fn same_object(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
