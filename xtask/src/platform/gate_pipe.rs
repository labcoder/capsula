//! The gate client's end of the brain's pipes, with overlapped I/O so one thread can block
//! reading while another writes; synchronous handles would serialize the two.

use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{HSTRING, PCWSTR};

/// A connected client end of a pipe. One thread may read while another writes; two concurrent
/// reads, or writes, must not happen.
#[derive(Debug)]
pub(crate) struct PipeClient {
    handle: OwnedHandle,
    aborted: AtomicBool,
}

impl PipeClient {
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
        })
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
        if self.aborted.load(Ordering::SeqCst) {
            // SAFETY: cancels only this operation; the wait below lets it finish before
            // `overlapped` and the buffer go away.
            let _done = unsafe { CancelIoEx(self.raw(), Some(&raw const overlapped)) };
        }
        // SAFETY: the event handle is open for the borrow.
        let _signalled = unsafe { WaitForSingleObject(HANDLE(event.as_raw_handle()), INFINITE) };
        let mut done = 0u32;
        // SAFETY: `overlapped` and the caller's buffer outlive the operation, because waiting
        // returns only once it has completed or been cancelled.
        unsafe { GetOverlappedResult(self.raw(), &raw const overlapped, &raw mut done, true) }?;
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

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The Windows session this process runs in.
pub(crate) fn session_id() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: plain call; `session` is a writable u32.
    unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) }?;
    Ok(session)
}
