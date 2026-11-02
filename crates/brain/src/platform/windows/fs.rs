//! File operations the store needs with guarantees std doesn't give.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

/// Moves `from` over `to` and returns only once the move is on disk. Unlike `std::fs::rename`,
/// it never falls back to replacing a file another process still has open.
///
/// # Errors
/// Fails when either file is held or the move fails.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Bytes this user may still write on the volume holding `dir`.
///
/// # Errors
/// Fails when the volume can't be queried.
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let dir = wide(dir);
    let mut free = 0;
    // SAFETY: `dir` is NUL-terminated and outlives the call; `free` is a writable u64 and the
    // totals aren't asked for.
    let queried = unsafe {
        GetDiskFreeSpaceExW(
            dir.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}

/// Whether a hard link failed because the volume has none, as on FAT and some network shares.
#[must_use]
pub fn links_unsupported(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        [ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED]
            .iter()
            .any(|unsupported| unsupported.cast_signed() == code)
    })
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
