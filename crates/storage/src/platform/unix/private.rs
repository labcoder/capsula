use std::fs::File;
use std::os::fd::AsRawFd as _;

use super::protected::stat;
use crate::Untrusted;

/// Opens a file read-capable but for its metadata only, as the owner approved. Nothing reads
/// through it: it stays an `OwnedFd`, which has no read methods. `O_NONBLOCK` keeps a FIFO
/// swapped in after the look from waiting for a writer, and `O_NOCTTY` keeps a terminal from
/// becoming the process's own.
pub(crate) const FILE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;

/// This process's user, when it isn't root and its real and effective identities match.
pub(crate) fn user() -> Result<libc::uid_t, Untrusted> {
    // SAFETY: getuid has no arguments or ownership effects.
    let user = unsafe { libc::getuid() };
    // SAFETY: geteuid only returns the effective identity.
    let effective = unsafe { libc::geteuid() };
    // SAFETY: getgid only returns the real group identity.
    let group = unsafe { libc::getgid() };
    // SAFETY: getegid only returns the effective group identity.
    let effective_group = unsafe { libc::getegid() };
    if user == 0 || user != effective || group != effective_group {
        return Err(Untrusted::Unsafe);
    }
    Ok(user)
}

/// The checked object is the one the look found, on its parent's device.
pub(crate) fn same_entry(
    parent: &File,
    seen: &libc::stat,
    checked: &libc::stat,
) -> Result<(), Untrusted> {
    if checked.st_dev != stat(parent.as_raw_fd())?.st_dev
        || checked.st_dev != seen.st_dev
        || checked.st_ino != seen.st_ino
    {
        return Err(Untrusted::Unsafe);
    }
    Ok(())
}
