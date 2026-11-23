use std::ffi::CStr;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::ptr::NonNull;

use super::{Fault, Stage, call};

// SDK sys/fcntl.h filesec_property_t values; libc doesn't declare these functions.
const FILESEC_OWNER: libc::c_int = 1;
const FILESEC_MODE: libc::c_int = 4;
const FILESEC_ACL: libc::c_int = 5;

// SAFETY: The declarations and the Intel inode64 symbol match Darwin's SDK.
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

pub(super) fn check_name(name: &CStr, stage: Stage) -> Result<(), Fault> {
    let bytes = name.to_bytes();
    if matches!(bytes, b"" | b"." | b"..") || bytes.contains(&b'/') {
        return Err(Fault::new(stage, ErrorKind::InvalidInput));
    }
    Ok(())
}

pub(super) fn open_at(
    parent: &File,
    name: &CStr,
    flags: libc::c_int,
    stage: Stage,
) -> Result<File, Fault> {
    check_name(name, stage)?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY;
    // SAFETY: The directory and the checked, terminated name stay live; mode is promoted to int.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600_i32) };
    if fd < 0 {
        return Err(Fault::io(stage, &io::Error::last_os_error()));
    }
    // SAFETY: Successful openat transferred a fresh descriptor, owned once by the File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat(fd: RawFd, stage: Stage) -> Result<libc::stat, Fault> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: fd is held, and metadata provides writable storage for a complete stat.
    call(unsafe { libc::fstat(fd, metadata.as_mut_ptr()) })
        .map_err(|error| Fault::io(stage, &error))?;
    // SAFETY: Successful fstat initialized the complete stat.
    Ok(unsafe { metadata.assume_init() })
}

pub(super) fn named_stat(parent: &File, name: &CStr, stage: Stage) -> Result<libc::stat, Fault> {
    check_name(name, stage)?;
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: The parent and the terminated name are held, and metadata is writable.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    call(result).map_err(|error| Fault::io(stage, &error))?;
    // SAFETY: Successful fstatat initialized the complete stat.
    Ok(unsafe { metadata.assume_init() })
}

pub(super) fn private_mode(metadata: &libc::stat, owner: libc::uid_t, mode: libc::mode_t) -> bool {
    metadata.st_uid == owner && metadata.st_mode & (libc::S_IFMT | 0o7777) == mode
}

fn same_inspection(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_uid == right.st_uid
        && left.st_gid == right.st_gid
        && left.st_mode == right.st_mode
        && left.st_nlink == right.st_nlink
}

pub(super) fn matches_name(
    parent: &File,
    name: &CStr,
    held: &libc::stat,
    stage: Stage,
) -> Result<(), Fault> {
    if !same_inspection(held, &named_stat(parent, name, stage)?) {
        return Err(Fault::new(stage, ErrorKind::InvalidData));
    }
    Ok(())
}

pub(super) fn inspect(file: &File, stage: Stage) -> Result<libc::stat, Fault> {
    let fd = file.as_raw_fd();
    let before = stat(fd, stage)?;
    Filesec::new(stage)?.inspect(fd, &before, stage)?;
    let after = stat(fd, stage)?;
    if !same_inspection(&before, &after) {
        return Err(Fault::new(stage, ErrorKind::InvalidData));
    }
    Ok(after)
}

struct Filesec(NonNull<libc::c_void>);

impl Filesec {
    fn new(stage: Stage) -> Result<Self, Fault> {
        // SAFETY: filesec_init takes no arguments and returns one owned filesec or null.
        NonNull::new(unsafe { filesec_init() })
            .map(Self)
            .ok_or_else(|| Fault::io(stage, &io::Error::last_os_error()))
    }

    fn inspect(&self, fd: RawFd, expected: &libc::stat, stage: Stage) -> Result<(), Fault> {
        // SAFETY: An all-zero stat is valid storage, not an observed result.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: The fd is borrowed, metadata is writable, and self owns a live filesec.
        call(unsafe { fstatx_np(fd, &raw mut metadata, self.0.as_ptr()) })
            .map_err(|error| Fault::io(stage, &error))?;
        if !same_inspection(expected, &metadata) {
            return Err(Fault::new(stage, ErrorKind::InvalidData));
        }
        let mut owner = libc::uid_t::MAX;
        // SAFETY: FILESEC_OWNER writes exactly a uid_t into live storage.
        call(unsafe {
            filesec_get_property(self.0.as_ptr(), FILESEC_OWNER, (&raw mut owner).cast())
        })
        .map_err(|error| Fault::io(stage, &error))?;
        let mut mode = libc::mode_t::MAX;
        // SAFETY: FILESEC_MODE writes exactly a mode_t into live storage.
        call(unsafe {
            filesec_get_property(self.0.as_ptr(), FILESEC_MODE, (&raw mut mode).cast())
        })
        .map_err(|error| Fault::io(stage, &error))?;
        if owner != expected.st_uid || mode != expected.st_mode {
            return Err(Fault::new(stage, ErrorKind::InvalidData));
        }
        let mut present = -1;
        // SAFETY: FILESEC_ACL writes a presence int, without allocating or returning an ACL.
        let result =
            unsafe { filesec_query_property(self.0.as_ptr(), FILESEC_ACL, &raw mut present) };
        acl_absent(call(result), present, stage)
    }
}

impl Drop for Filesec {
    fn drop(&mut self) {
        // SAFETY: This is the sole owner; filesec_free returns void and runs once.
        unsafe { filesec_free(self.0.as_ptr()) };
    }
}

pub(super) fn acl_absent(
    result: io::Result<()>,
    present: libc::c_int,
    stage: Stage,
) -> Result<(), Fault> {
    result.map_err(|error| Fault::io(stage, &error))?;
    match present.cmp(&0) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(Fault::new(stage, ErrorKind::InvalidData)),
        std::cmp::Ordering::Greater => Err(Fault::new(stage, ErrorKind::PermissionDenied)),
    }
}

pub(super) fn unlink(
    parent: &File,
    name: &CStr,
    held: &libc::stat,
    flags: libc::c_int,
) -> Result<(), Fault> {
    matches_name(parent, name, held, Stage::Cleanup)?;
    // SAFETY: The verified name and parent stay live; only that entry is removed.
    call(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) })
        .map_err(|error| Fault::io(Stage::Cleanup, &error))
}
