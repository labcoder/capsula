//! The current process's user SID.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The SID of the user this process runs as, in string form (`S-1-5-21-…`).
///
/// # Errors
/// Fails if the process token or its user SID cannot be read or converted.
///
/// # Panics
/// Panics if the token buffer length cannot fit in `usize`.
pub fn user_sid() -> io::Result<String> {
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
