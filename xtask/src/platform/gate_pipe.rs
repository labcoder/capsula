//! The gate client's end of the brain's pipes, with overlapped I/O so one thread can block
//! reading while another writes; synchronous handles would serialize the two.

use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_BUSY, ERROR_TIMEOUT, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{HSTRING, PCWSTR};

/// How long one write may take before it's cancelled and fails with `TimedOut`.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected client end of a pipe. One thread may read while another writes; two concurrent
/// reads, or writes, must not happen.
#[derive(Debug)]
pub(crate) struct PipeClient {
    handle: OwnedHandle,
    aborted: AtomicBool,
    /// Milliseconds each read may take; `INFINITE` by default.
    read_timeout_ms: AtomicU32,
    deadline: Option<Instant>,
}

impl PipeClient {
    /// The process serving this open handle.
    pub(crate) fn server_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        // SAFETY: the handle is borrowed and the PID output is writable.
        unsafe { GetNamedPipeServerProcessId(self.raw(), &raw mut pid) }?;
        Ok(pid)
    }

    pub(crate) fn require_server(&self, pid: u32) -> io::Result<()> {
        if self.server_pid()? != pid {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }

    /// Connects to the pipe `name`.
    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let wide = HSTRING::from(name);
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
        .map_err(io::Error::from)?;
        // SAFETY: CreateFileW returned a valid handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self {
            handle,
            aborted: AtomicBool::new(false),
            read_timeout_ms: AtomicU32::new(INFINITE),
            deadline: None,
        })
    }

    pub(crate) fn until(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub(crate) fn read_stamp(&self) -> Option<Instant> {
        self.deadline.map(|_| Instant::now())
    }

    /// Bounds each later read; `None` waits until the pipe ends or is aborted.
    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) {
        self.read_timeout_ms
            .store(timeout.map_or(INFINITE, millis), Ordering::Relaxed);
    }

    /// Fails the transfers under way and every later one, so threads blocked on the pipe return.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // SAFETY: cancels every transfer on this handle; each waits for its own operation to
        // finish before its buffer and `OVERLAPPED` go away (see `transfer`).
        let _nothing_pending = unsafe { CancelIoEx(self.raw(), None) };
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// Runs one overlapped read or write and waits for it, at most `timeout_ms`: then it's
    /// cancelled, and still waited for, so it never outlives its buffer. `start` must use the
    /// buffer the caller borrows for the whole call.
    fn transfer(
        &self,
        timeout_ms: u32,
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
        let absolute = if let Some(end) = self.deadline {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ERROR_TIMEOUT.to_hresult().into());
            }
            (timeout_ms == INFINITE || remaining <= Duration::from_millis(u64::from(timeout_ms)))
                .then_some(end)
        } else {
            None
        };
        if let Err(error) = start(self.raw(), &raw mut overlapped)
            && error.code() != ERROR_IO_PENDING.to_hresult()
        {
            return Err(error);
        }
        if self.aborted.load(Ordering::SeqCst) {
            // SAFETY: cancels only this operation; the wait below lets it finish before
            // `overlapped` and the buffer go away.
            let _done = unsafe { CancelIoEx(self.raw(), Some(&raw const overlapped)) };
        }
        let waited = loop {
            let wait_ms = absolute.map_or(timeout_ms, |end| {
                millis(end.saturating_duration_since(Instant::now())).clamp(1, INFINITE - 1)
            });
            // SAFETY: the event handle is open for the borrow.
            let waited = unsafe { WaitForSingleObject(HANDLE(event.as_raw_handle()), wait_ms) };
            if waited != WAIT_TIMEOUT || absolute.is_none_or(|end| Instant::now() >= end) {
                break waited;
            }
        };
        if waited == WAIT_TIMEOUT {
            // SAFETY: cancels only this operation; the wait below lets it finish before
            // `overlapped` and the buffer go away.
            let _done = unsafe { CancelIoEx(self.raw(), Some(&raw const overlapped)) };
        }
        let mut done = 0u32;
        // SAFETY: `overlapped` and the caller's buffer outlive the operation, because waiting
        // returns only once it has completed or been cancelled.
        let result =
            unsafe { GetOverlappedResult(self.raw(), &raw const overlapped, &raw mut done, true) };
        if waited != WAIT_OBJECT_0 && result.is_err() {
            return Err(ERROR_TIMEOUT.to_hresult().into());
        }
        result?;
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(ERROR_TIMEOUT.to_hresult().into());
        }
        Ok(usize::try_from(done).expect("u32 fits in usize on Windows"))
    }
}

impl Read for &PipeClient {
    /// A closed server reads as end of stream.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = self.read_timeout_ms.load(Ordering::Relaxed);
        // SAFETY: `buf` outlives the operation (see `transfer`); `overlapped` is valid.
        match self.transfer(timeout, |handle, overlapped| unsafe {
            ReadFile(handle, Some(buf), None, Some(overlapped))
        }) {
            Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            result => result.map_err(pipe_io),
        }
    }
}

impl Write for &PipeClient {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` outlives the operation (see `transfer`); `overlapped` is valid.
        self.transfer(millis(WRITE_TIMEOUT), |handle, overlapped| unsafe {
            WriteFile(handle, Some(buf), None, Some(overlapped))
        })
        .map_err(pipe_io)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether a server is listening on the pipe `name`, checked without connecting to it.
pub(crate) fn listening(name: &str) -> bool {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { WaitNamedPipeW(&wide, 1) }.as_bool()
}

/// The Windows session this process runs in.
pub(crate) fn session_id() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: plain call; `session` is a writable u32.
    unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) }?;
    Ok(session)
}

pub(crate) fn is_starting(error: &io::Error) -> bool {
    [ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY]
        .iter()
        .any(|code| error.raw_os_error() == Some(code.to_hresult().0))
}

fn pipe_io(error: windows::core::Error) -> io::Error {
    if error.code() == ERROR_TIMEOUT.to_hresult() {
        io::ErrorKind::TimedOut.into()
    } else {
        error.into()
    }
}

fn millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1)
}

#[cfg(test)]
pub(crate) fn fixture_server(name: &str) -> std::fs::File {
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    };

    let wide = HSTRING::from(name);
    // SAFETY: the name is live for the call; default security is sufficient for the local fixture.
    let server = unsafe {
        CreateNamedPipeW(
            &wide,
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            2,
            4096,
            4096,
            0,
            None,
        )
    };
    assert!(!server.is_invalid(), "one local fixture server");
    // SAFETY: the successful call returned a new, exclusively owned server handle.
    let server = unsafe { OwnedHandle::from_raw_handle(server.0) };
    std::fs::File::from(server)
}

#[cfg(test)]
pub(crate) fn fixture_available(server: &std::fs::File) -> io::Result<u32> {
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, PeekNamedPipe};

    let server = HANDLE(server.as_raw_handle());
    let mut client = 0;
    // SAFETY: the server handle is borrowed and `client` is writable.
    unsafe { GetNamedPipeClientProcessId(server, &raw mut client) }?;
    assert_eq!(
        client,
        std::process::id(),
        "the connected client is this test"
    );
    let mut bytes = 0;
    // SAFETY: the server handle is borrowed, no buffer is supplied, and `bytes` is writable.
    unsafe { PeekNamedPipe(server, None, 0, None, Some(&raw mut bytes), None) }?;
    Ok(bytes)
}
