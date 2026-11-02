//! Owner-only named pipes, the client's identity, and the session id.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// The brain's end of a pipe.
pub type PipeServer = NamedPipeServer;

/// A test's client end of a pipe.
#[cfg(test)]
pub(crate) type PipeClient = tokio::net::windows::named_pipe::NamedPipeClient;

/// A connected owner-only pipe and its client, under a name no running brain uses, for tests
/// over a real pipe.
///
/// # Errors
/// Fails when the OS refuses the pipe or the connection.
#[cfg(test)]
pub(crate) async fn test_pair(name: &str) -> io::Result<(NamedPipeServer, PipeClient)> {
    let name = format!(
        r"\\.\pipe\{}-test-{}-{name}",
        capsula_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    let server = create_owner_only(&name, Instance::First)?;
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&name)?;
    server.connect().await?;
    Ok((server, client))
}

/// Whether a pipe instance is the first with its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instance {
    /// Fails if any process already holds the name.
    First,
    /// Replaces an instance this process holds, so the name is never free in between.
    Next,
}

/// Creates an instance of `name`, refusing remote clients and every other user. At most two
/// instances exist: the one in use and its replacement while it's swapped in.
///
/// # Errors
/// Fails when the name is taken (another brain got there first), the OS refuses, or the DACL
/// read back from the created pipe isn't the owner-only one.
#[expect(
    clippy::missing_panics_doc,
    reason = "the only expect is on a fixed 24-byte struct size"
)]
pub fn create_owner_only(name: &str, instance: Instance) -> io::Result<NamedPipeServer> {
    let user = user_sid()?;
    // A protected DACL with one entry: full access for this user's own SID. Without it, the
    // default DACL also lets Everyone and anonymous clients read. `OW` (owner) isn't enough,
    // because an elevated token can make Administrators the owner.
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{user})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
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
            .first_pipe_instance(instance == Instance::First)
            .max_instances(2)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    // The OS writes some SIDs as aliases (the built-in Administrator account as `LA`), so the
    // check compares against how it writes ours, not the raw S-1-5 form.
    let wanted = descriptor_sddl(descriptor);
    // SAFETY: the descriptor came from the conversion above and nothing references it now.
    unsafe { LocalFree(descriptor) };
    let server = server?;
    let wanted = wanted?;
    let user = wanted
        .rsplit(';')
        .next()
        .and_then(|sid| sid.strip_suffix(')'))
        .unwrap_or(&user);
    check_user_only(&dacl_sddl(&server)?, user)
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
    let result = descriptor_sddl(descriptor);
    // SAFETY: `descriptor` came from GetSecurityInfo and is no longer referenced.
    unsafe { LocalFree(descriptor) };
    result
}

/// A security descriptor's DACL in SDDL.
fn descriptor_sddl(descriptor: PSECURITY_DESCRIPTOR) -> io::Result<String> {
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
    result
}

/// The SID of the user this process runs as, in string form (`S-1-5-21-…`).
fn user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: plain call returning a pseudo-handle that needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `token` receives a handle owned below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a fresh handle owned only here.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0u32;
    // SAFETY: the sizing call writes only `len`; its expected failure is ignored.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut len,
        )
    };
    // u64s, so the TOKEN_USER at the start is aligned.
    let mut buffer = vec![0u64; usize::try_from(len).expect("u32 fits in usize").div_ceil(8)];
    // SAFETY: `buffer` holds at least `len` writable bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of the aligned buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` points into `buffer`, alive here; on success `text` is LocalAlloc'd.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SID strings are under 200 characters; the bound only stops a runaway scan.
    let len = (0..256)
        .take_while(|&i| {
            // SAFETY: `text` is NUL-terminated and `i` never passes the NUL.
            let unit = unsafe { text.add(i) };
            // SAFETY: as above.
            unsafe { *unit != 0 }
        })
        .count();
    // SAFETY: the `len` units before the NUL are initialized.
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: `text` came from ConvertSidToStringSidW and is no longer referenced.
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// Accepts a protected DACL with exactly one allow entry, for `user`.
fn check_user_only(sddl: &str, user: &str) -> Result<(), String> {
    let dacl = sddl
        .strip_prefix("D:")
        .ok_or_else(|| format!("no DACL in {sddl}"))?;
    let (flags, aces) = dacl.split_at(dacl.find('(').unwrap_or(dacl.len()));
    let fields: Vec<&str> = aces
        .strip_prefix('(')
        .and_then(|a| a.strip_suffix(')'))
        .map(|a| a.split(';').collect())
        .unwrap_or_default();
    let only_user = matches!(fields.as_slice(), ["A", _, rights, _, _, sid] if !rights.is_empty() && *sid == user);
    if flags.contains('P') && only_user && aces.get(1..).is_some_and(|rest| !rest.contains('(')) {
        Ok(())
    } else {
        Err(format!("pipe DACL is not user-only: {sddl}"))
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
    use super::{Instance, check_user_only, create_owner_only, user_sid};

    const ME: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn only_a_protected_single_user_entry_passes() {
        assert!(
            check_user_only("D:P(A;;FA;;;S-1-5-21-1-2-3-1001)", ME).is_ok(),
            "user only"
        );
        assert!(
            check_user_only("D:(A;;FA;;;S-1-5-21-1-2-3-1001)", ME).is_err(),
            "unprotected"
        );
        assert!(
            check_user_only("D:P(A;;FA;;;S-1-5-21-1-2-3-1001)(A;;FR;;;WD)", ME).is_err(),
            "a second entry"
        );
        assert!(
            check_user_only("D:P(A;;FA;;;OW)", ME).is_err(),
            "the owner alias"
        );
        assert!(
            check_user_only("D:P(A;;FA;;;S-1-5-21-1-2-3-1002)", ME).is_err(),
            "another user"
        );
        assert!(check_user_only("D:P", ME).is_err(), "empty DACL");
        assert!(check_user_only("O:BA", ME).is_err(), "no DACL");
    }

    #[test]
    fn the_user_sid_is_a_string_sid() {
        let sid = user_sid().expect("this process has a user");
        assert!(sid.starts_with("S-1-5-"), "{sid}");
    }
    #[tokio::test]
    async fn created_pipe_is_owner_only_and_unique() {
        let name = format!(r"\\.\pipe\capsula-test-{}", std::process::id());
        let first = create_owner_only(&name, Instance::First).expect("first instance");
        let again = create_owner_only(&name, Instance::First);
        assert!(again.is_err(), "a second first instance must fail");
        let next = create_owner_only(&name, Instance::Next).expect("replacement instance");
        let third = create_owner_only(&name, Instance::Next);
        assert!(third.is_err(), "only two instances may exist");
        drop((first, next));
    }
}
