//! The shell's end of a named pipe, with overlapped I/O so one thread can block reading while
//! another writes. Plain synchronous handles serialize the two, so a pending read would hold
//! back `Quitting`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_TIMEOUT, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// A connected client end of a pipe. Reading and writing through `&PipeClient` may happen on
/// two threads at once; two concurrent reads (or writes) would interleave and must not.
#[derive(Debug)]
pub(crate) struct PipeClient {
    handle: OwnedHandle,
    /// Milliseconds each read or write may take; `INFINITE` by default.
    timeout_ms: AtomicU32,
    /// Set by [`PipeClient::abort`]: every transfer fails from then on.
    aborted: AtomicBool,
}

impl PipeClient {
    /// Connects to the pipe `name`.
    ///
    /// # Errors
    /// `NotFound` when no server has created it, `ERROR_PIPE_BUSY` (231) when another client
    /// holds it, or any other OS error.
    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let wide = HSTRING::from(name);
        // Identification level only, so the brain can check who we are but never act as us.
        let flags = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
        // SAFETY: `wide` is NUL-terminated and outlives the call; no template or attributes.
        let handle = unsafe {
            CreateFileW(
                &wide,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
        }
        .map_err(win32_io_error)?;
        // SAFETY: CreateFileW returned a valid handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self {
            handle,
            timeout_ms: AtomicU32::new(INFINITE),
            aborted: AtomicBool::new(false),
        })
    }

    /// Fails the transfers under way on any thread, and every later one, so the threads blocked
    /// on this pipe return. The pipe stays open until it's dropped.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // SAFETY: cancels every transfer on this handle; each waits for its own operation to
        // finish before its buffer and `OVERLAPPED` go away (see `transfer`).
        if let Err(error) = unsafe { CancelIoEx(self.raw(), None) } {
            tracing::debug!(%error, "no transfer was under way to cancel");
        }
    }

    /// The executable of the process serving this pipe.
    ///
    /// # Errors
    /// Fails when the server's process can't be queried.
    pub(crate) fn server_image(&self) -> io::Result<PathBuf> {
        let mut pid = 0;
        // SAFETY: the handle is open for the borrow and `pid` is a writable u32.
        unsafe { GetNamedPipeServerProcessId(self.raw(), &raw mut pid) }?;
        process_image(pid)
    }

    /// Bounds each later read and write; `None` waits forever. A transfer that times out is
    /// cancelled and fails with `TimedOut`.
    pub(crate) fn set_timeout(&self, timeout: Option<Duration>) {
        let ms = timeout.map_or(INFINITE, |t| {
            u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1)
        });
        self.timeout_ms.store(ms, Ordering::Relaxed);
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// Runs one overlapped read or write and waits for it. `start` must use the buffer the
    /// caller borrows for the whole call, so it outlives the operation.
    fn transfer(
        &self,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> windows::core::Result<usize> {
        // SAFETY: plain call creating an unnamed manual-reset event.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        // SAFETY: the event handle is valid and owned only here.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let mut overlapped = OVERLAPPED {
            hEvent: HANDLE(event.as_raw_handle()),
            ..Default::default()
        };
        if self.aborted.load(Ordering::SeqCst) {
            return Err(ERROR_OPERATION_ABORTED.to_hresult().into());
        }
        if let Err(error) = start(self.raw(), &raw mut overlapped)
            && error.code() != ERROR_IO_PENDING.to_hresult()
        {
            return Err(error);
        }
        // An abort between the check above and the start found nothing to cancel.
        if self.aborted.load(Ordering::SeqCst) {
            // SAFETY: cancels only this operation; the wait below lets it finish before
            // `overlapped` and the buffer go away.
            if let Err(error) = unsafe { CancelIoEx(self.raw(), Some(&raw const overlapped)) } {
                tracing::debug!(%error, "cancel found the transfer already done");
            }
        }
        // SAFETY: the event handle is open for the borrow.
        let waited = unsafe {
            WaitForSingleObject(
                HANDLE(event.as_raw_handle()),
                self.timeout_ms.load(Ordering::Relaxed),
            )
        };
        if waited == WAIT_TIMEOUT {
            // SAFETY: cancels only this operation; the wait below lets it finish before
            // `overlapped` and the buffer go away.
            if let Err(error) = unsafe { CancelIoEx(self.raw(), Some(&raw const overlapped)) } {
                tracing::debug!(%error, "cancel found the transfer already done");
            }
        }
        let mut done = 0u32;
        // SAFETY: `overlapped` and the caller's buffer outlive the operation, because waiting
        // (`bwait` true) returns only once it has completed or been cancelled.
        let result =
            unsafe { GetOverlappedResult(self.raw(), &raw const overlapped, &raw mut done, true) };
        if waited != WAIT_OBJECT_0 && result.is_err() {
            return Err(ERROR_TIMEOUT.to_hresult().into());
        }
        result?;
        Ok(usize::try_from(done).expect("u32 fits in usize on Windows"))
    }
}

impl Read for &PipeClient {
    /// A closed server reads as end of stream.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` outlives the operation (see `transfer`); `overlapped` is valid.
        match self.transfer(|handle, overlapped| unsafe {
            ReadFile(handle, Some(buf), None, Some(overlapped))
        }) {
            Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            result => result.map_err(io::Error::from),
        }
    }
}

impl Write for &PipeClient {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` outlives the operation (see `transfer`); `overlapped` is valid.
        self.transfer(|handle, overlapped| unsafe {
            WriteFile(handle, Some(buf), None, Some(overlapped))
        })
        .map_err(io::Error::from)
    }

    /// Waits until the brain has read everything written.
    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: the handle is open for the borrow.
        unsafe { FlushFileBuffers(self.raw()) }.map_err(io::Error::from)
    }
}

fn process_image(pid: u32) -> io::Result<PathBuf> {
    // SAFETY: plain call; failure is returned as an error.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
    // SAFETY: `raw` is a process handle owned only here; OwnedHandle closes it once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = u32::try_from(buffer.len()).expect("32K fits in u32");
    // SAFETY: `buffer` holds `len` writable units, and `len` is updated to the units written.
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    }?;
    buffer.truncate(usize::try_from(len).expect("u32 fits in usize on Windows"));
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

/// The server and client ends of a new pipe, for tests that play the brain. The server end uses
/// the brain's 64 KiB buffers.
///
/// # Errors
/// Fails when the pipe can't be created or opened.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn test_pair(name: &str) -> io::Result<(PipeClient, PipeClient)> {
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call; no security attributes.
    let server = unsafe {
        CreateNamedPipeW(
            &wide,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            None,
        )
    };
    if server.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateNamedPipeW returned a valid handle that nothing else owns.
    let server = unsafe { OwnedHandle::from_raw_handle(server.0) };
    let client = PipeClient::open(name)?;
    let server = PipeClient {
        handle: server,
        timeout_ms: AtomicU32::new(INFINITE),
        aborted: AtomicBool::new(false),
    };
    Ok((server, client))
}

/// The Windows session this process runs in.
///
/// # Errors
/// Fails when the OS can't report it.
pub fn session_id() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: plain call; `session` is a writable u32.
    unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) }?;
    Ok(session)
}

/// Converts a Win32 failure to an `io::Error` with the plain Win32 code, so `kind()` reports
/// `NotFound` and friends; `io::Error::from` keeps the HRESULT, which maps to no kind.
fn win32_io_error(error: windows::core::Error) -> io::Error {
    let hresult = error.code().0.cast_unsigned();
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error(i32::try_from(hresult & 0xFFFF).expect("16 bits fit in i32"))
    } else {
        error.into()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::PipeClient;

    #[test]
    fn a_missing_pipe_is_not_found() {
        let error = PipeClient::open(&format!(
            r"\\.\pipe\{}-test-no-such-pipe",
            kumochi_protocol::constants::OBJECT_PREFIX
        ))
        .expect_err("no server");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error:?}");
    }
}
