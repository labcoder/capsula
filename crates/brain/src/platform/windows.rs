//! Win32: owner-only named pipes, the client's identity, and the session id.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// The brain's end of a pipe.
pub type PipeServer = NamedPipeServer;

/// A protected DACL with one entry: full access for the pipe's owner, the user who made it.
/// Without it, the default DACL also lets Everyone and anonymous clients read.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

/// Creates the only instance of `name`, refusing remote clients and every other user.
///
/// # Errors
/// Fails when the name is taken (another brain got there first), the OS refuses, or the DACL
/// read back from the created pipe isn't the owner-only one.
#[expect(
    clippy::missing_panics_doc,
    reason = "the only expect is on a fixed 24-byte struct size"
)]
pub fn create_owner_only(name: &str) -> io::Result<NamedPipeServer> {
    let sddl: Vec<u16> = OWNER_ONLY.encode_utf16().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; on success `descriptor` holds a
    // LocalAlloc'd buffer that is freed below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("a 24-byte struct"),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` and the descriptor it points to are valid for the whole call.
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    // SAFETY: the descriptor came from the conversion above and nothing references it now.
    unsafe { LocalFree(descriptor) };
    let server = server?;
    check_owner_only(&dacl_sddl(&server)?)
        .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))?;
    Ok(server)
}

/// The DACL the OS actually holds for this pipe, read from the server's own handle.
fn dacl_sddl(server: &NamedPipeServer) -> io::Result<String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle is open for the borrow; on success `descriptor` is LocalAlloc'd and freed below.
    let status = unsafe {
        GetSecurityInfo(
            server.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).expect("Win32 error codes fit in i32"),
        ));
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `descriptor` is valid; on success `text` holds `len` LocalAlloc'd UTF-16 units.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            &raw mut len,
        )
    };
    let result = if converted == 0 {
        Err(io::Error::last_os_error())
    } else {
        let len = usize::try_from(len).expect("u32 fits in usize on Windows");
        // SAFETY: the conversion wrote `len` units at `text`.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        Ok(String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_owned())
    };
    // SAFETY: `text` came from the conversion above (or is null) and is no longer referenced.
    unsafe { LocalFree(text.cast()) };
    // SAFETY: `descriptor` came from GetSecurityInfo and is no longer referenced.
    unsafe { LocalFree(descriptor) };
    result
}

/// Accepts a protected DACL with exactly one allow entry, for the owner.
fn check_owner_only(sddl: &str) -> Result<(), String> {
    let dacl = sddl
        .strip_prefix("D:")
        .ok_or_else(|| format!("no DACL in {sddl}"))?;
    let (flags, aces) = dacl.split_at(dacl.find('(').unwrap_or(dacl.len()));
    let fields: Vec<&str> = aces
        .strip_prefix('(')
        .and_then(|a| a.strip_suffix(')'))
        .map(|a| a.split(';').collect())
        .unwrap_or_default();
    let owner =
        matches!(fields.as_slice(), ["A", _, rights, _, _, "OW" | "S-1-3-4"] if !rights.is_empty());
    if flags.contains('P') && owner && aces.get(1..).is_some_and(|rest| !rest.contains('(')) {
        Ok(())
    } else {
        Err(format!("pipe DACL is not owner-only: {sddl}"))
    }
}

/// The executable of the process connected to `server`.
///
/// # Errors
/// Fails when no client is connected or its process can't be queried.
pub fn client_image(server: &NamedPipeServer) -> io::Result<PathBuf> {
    let mut pid = 0;
    // SAFETY: the handle is open for the borrow and `pid` is a writable u32.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &raw mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    process_image(pid)
}

fn process_image(pid: u32) -> io::Result<PathBuf> {
    // SAFETY: plain call; a null result is checked below.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a process handle we own; OwnedHandle closes it once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = u32::try_from(buffer.len()).expect("32K fits in u32");
    // SAFETY: `buffer` holds `len` writable units, and `len` is updated to the units written.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(usize::try_from(len).expect("u32 fits in usize on Windows"));
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

/// The Windows session this process runs in.
///
/// # Errors
/// Fails when the OS can't report it.
pub fn session_id() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: plain call; `session` is a writable u32.
    if unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::{check_owner_only, create_owner_only};

    #[test]
    fn only_a_protected_single_owner_entry_passes() {
        assert!(check_owner_only("D:P(A;;FA;;;OW)").is_ok(), "owner only");
        assert!(check_owner_only("D:(A;;FA;;;OW)").is_err(), "unprotected");
        assert!(
            check_owner_only("D:P(A;;FA;;;OW)(A;;FR;;;WD)").is_err(),
            "a second entry"
        );
        assert!(check_owner_only("D:P(A;;FA;;;WD)").is_err(), "everyone");
        assert!(check_owner_only("D:P").is_err(), "empty DACL");
        assert!(check_owner_only("O:BA").is_err(), "no DACL");
    }

    #[tokio::test]
    async fn created_pipe_is_owner_only_and_unique() {
        let name = format!(r"\\.\pipe\capsula-test-{}", std::process::id());
        let first = create_owner_only(&name).expect("first instance");
        assert!(
            create_owner_only(&name).is_err(),
            "a second instance must fail"
        );
        drop(first);
    }
}
