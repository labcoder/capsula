//! Shell-owned folder discovery and compatibility exports for protected filesystem operations.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::PathBuf;

use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

#[cfg(test)]
pub(crate) use kumochi_storage::{
    DaclEntry, dacl_entries, dacl_sddl, final_path_matches, pause_after_check, set_dacl, this_user,
};
pub(crate) use kumochi_storage::{
    Untrusted, hold_chain, hold_file, mark, marker, owner_only_dir, private_tree, remove_tree,
    unmark,
};

/// The local application data folder as the shell knows it, not as the environment says.
pub(crate) fn local_app_data() -> io::Result<PathBuf> {
    known_folder(FOLDERID_LocalAppData)
}

/// The user's profile folder as the shell knows it, not as `USERPROFILE` says.
pub(crate) fn profile_dir() -> io::Result<PathBuf> {
    known_folder(FOLDERID_Profile)
}

fn known_folder(folder: windows_sys::core::GUID) -> io::Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success `path` holds a CoTaskMemAlloc'd, NUL-terminated string freed below.
    let status = unsafe {
        SHGetKnownFolderPath(
            &raw const folder,
            KF_FLAG_DEFAULT.cast_unsigned(),
            std::ptr::null_mut(),
            &raw mut path,
        )
    };
    let read = if status < 0 || path.is_null() {
        Err(io::Error::from_raw_os_error(status))
    } else {
        let mut len = 0;
        loop {
            // SAFETY: the string is NUL-terminated and `len` hasn't passed its NUL.
            let unit = unsafe { path.add(len) };
            // SAFETY: as above, `unit` points inside the string.
            if unsafe { *unit } == 0 {
                break;
            }
            len += 1;
        }
        // SAFETY: `path` holds `len` readable units before its NUL.
        let units = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(OsString::from_wide(units)))
    };
    // SAFETY: `path` is null or the buffer the shell allocated, freed once.
    unsafe { CoTaskMemFree(path.cast()) };
    read
}
