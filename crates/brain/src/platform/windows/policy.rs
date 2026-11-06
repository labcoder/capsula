//! Claude Code's managed-policy sources, checked by existence only, and the system values a
//! child's environment needs, read from the OS rather than the brain's own environment.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemDirectoryW, GetSystemWindowsDirectoryW,
};

use crate::platform::{ManagedPresence, Presence};

/// The documented endpoint-managed files (Claude Code's managed-settings page, read 2026-11-04).
const FILES: [&str; 3] = [
    r"C:\Program Files\ClaudeCode\managed-settings.json",
    r"C:\Program Files\ClaudeCode\managed-settings.d",
    r"C:\Program Files\ClaudeCode\managed-mcp.json",
];
const KEY: &str = r"SOFTWARE\Policies\ClaudeCode";
const VALUE: &str = "Settings";

/// Checks the three files and the `Settings` value under the machine's and the user's policy
/// key. It never enumerates a folder or key and never reads a value's data.
pub fn managed_presence() -> ManagedPresence {
    ManagedPresence {
        files: FILES.map(|path| file_presence(Path::new(path))),
        machine: value_presence(HKEY_LOCAL_MACHINE, KEY, VALUE),
        user: value_presence(HKEY_CURRENT_USER, KEY, VALUE),
    }
}

fn missing_or_indeterminate(code: u32) -> Presence {
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        Presence::Absent
    } else {
        Presence::Indeterminate
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

fn file_presence(path: &Path) -> Presence {
    let path = wide(path.as_os_str());
    // SAFETY: the path is NUL-terminated and outlives the call.
    let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
    if attributes != INVALID_FILE_ATTRIBUTES {
        return Presence::Present;
    }
    // SAFETY: no preconditions; it reads this thread's last error, which the call above set.
    missing_or_indeterminate(unsafe { GetLastError() })
}

fn value_presence(hive: HKEY, key: &str, value: &str) -> Presence {
    let key = wide(OsStr::new(key));
    let value = wide(OsStr::new(value));
    let mut opened: HKEY = std::ptr::null_mut();
    // SAFETY: the key name is NUL-terminated and outlives the call; `opened` is a valid out-pointer.
    let status = unsafe { RegOpenKeyExW(hive, key.as_ptr(), 0, KEY_QUERY_VALUE, &raw mut opened) };
    if status != ERROR_SUCCESS {
        return missing_or_indeterminate(status);
    }
    // SAFETY: `opened` is open; the null type, data and size pointers ask for existence only.
    let status = unsafe {
        RegQueryValueExW(
            opened,
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `opened` came from RegOpenKeyExW above and is closed once.
    unsafe { RegCloseKey(opened) };
    if status == ERROR_SUCCESS {
        Presence::Present
    } else {
        missing_or_indeterminate(status)
    }
}

/// `SystemRoot`, `SystemDrive` and `PATH` (the system folder, then the Windows folder), from the
/// OS. Claude Code starts `where.exe` and `reg.exe`, which live there.
///
/// # Errors
/// When the OS doesn't give the folders, or the Windows folder has no drive letter.
pub fn system_env() -> io::Result<Vec<(OsString, OsString)>> {
    // SAFETY: `buf` holds `len` writable units for the duration of each call.
    let windows = folder(|buf, len| unsafe { GetSystemWindowsDirectoryW(buf, len) })?;
    // SAFETY: as above.
    let system = folder(|buf, len| unsafe { GetSystemDirectoryW(buf, len) })?;
    let units: Vec<u16> = windows.encode_wide().collect();
    let drive = match units.as_slice() {
        [letter, colon, ..]
            if *colon == u16::from(b':')
                && u8::try_from(*letter).is_ok_and(|byte| byte.is_ascii_alphabetic()) =>
        {
            OsString::from_wide(&units[..2])
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the Windows folder has no drive letter",
            ));
        }
    };
    let mut path = system;
    path.push(";");
    path.push(&windows);
    Ok(vec![
        ("SystemRoot".into(), windows),
        ("SystemDrive".into(), drive),
        ("PATH".into(), path),
    ])
}

/// The program Claude Code's `auth login` opens its sign-in page with. A second `explorer.exe`
/// hands the URL to the running shell, which opens the default browser in the user's own
/// environment, so the browser never inherits the launch's owned folders.
///
/// # Errors
/// When the OS doesn't give the Windows folder.
pub fn login_browser() -> io::Result<OsString> {
    // SAFETY: `buf` holds `len` writable units for the duration of each call.
    let mut path = folder(|buf, len| unsafe { GetSystemWindowsDirectoryW(buf, len) })?;
    path.push(r"\explorer.exe");
    Ok(path)
}

/// Calls `read` with a growing buffer until the folder fits.
fn folder(read: impl Fn(*mut u16, u32) -> u32) -> io::Result<OsString> {
    let mut buf = vec![0_u16; 261];
    loop {
        let len =
            u32::try_from(buf.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        let wrote = usize::try_from(read(buf.as_mut_ptr(), len))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        if wrote == 0 {
            return Err(io::Error::last_os_error());
        }
        if wrote < buf.len() {
            buf.truncate(wrote);
            return Ok(OsString::from_wide(&buf));
        }
        buf.resize(wrote, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_proven_missing_file_or_folder_is_absent() {
        let dir = std::env::temp_dir().join(format!(
            "{}-policy-test-{}",
            kumochi_protocol::constants::OBJECT_PREFIX,
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("managed-settings.d")).expect("scratch folder");
        std::fs::write(dir.join("managed-settings.json"), b"").expect("scratch file");
        let cases = [
            (dir.join("managed-settings.json"), Presence::Present),
            (dir.join("managed-settings.d"), Presence::Present),
            (dir.join("managed-mcp.json"), Presence::Absent),
            (
                dir.join("missing").join("managed-mcp.json"),
                Presence::Absent,
            ),
            (dir.join("bad<name"), Presence::Indeterminate),
        ];
        let seen: Vec<Presence> = cases.iter().map(|(path, _)| file_presence(path)).collect();
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(
            seen,
            cases.map(|(_, presence)| presence),
            "file, folder, missing, missing parent, bad name"
        );
    }

    #[test]
    fn only_a_proven_missing_key_or_value_is_absent() {
        let present_key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let missing_key = format!(
            r"SOFTWARE\{}-test-missing",
            kumochi_protocol::constants::OBJECT_PREFIX
        );
        assert_eq!(
            value_presence(HKEY_LOCAL_MACHINE, present_key, "ProductName"),
            Presence::Present
        );
        assert_eq!(
            value_presence(HKEY_LOCAL_MACHINE, present_key, "kumochi-test-missing"),
            Presence::Absent
        );
        assert_eq!(
            value_presence(HKEY_CURRENT_USER, &missing_key, VALUE),
            Presence::Absent
        );
        assert_eq!(
            value_presence(std::ptr::null_mut(), KEY, VALUE),
            Presence::Indeterminate,
            "a bad hive"
        );
    }

    #[test]
    #[ignore = "observes Rez's real managed sources; run by hand: cargo test -p kumochi-brain host_managed -- --ignored --nocapture"]
    fn host_managed_presence() {
        let presence = managed_presence();
        assert!(
            presence
                .all()
                .iter()
                .all(|source| *source != Presence::Indeterminate),
            "{presence:?}"
        );
    }
}
