//! The shell's end of a named pipe, with overlapped I/O so one thread can block reading while
//! another writes. Plain synchronous handles serialize the two, so a pending read would hold
//! back `Quitting`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;

use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// A connected client end of a pipe. Reading and writing through `&PipeClient` may happen on
/// two threads at once; two concurrent reads (or writes) would interleave and must not.
#[derive(Debug)]
pub(crate) struct PipeClient(OwnedHandle);

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
        }?;
        // SAFETY: CreateFileW returned a valid handle that nothing else owns.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.0) }))
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

    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
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
        if let Err(error) = start(self.raw(), &raw mut overlapped)
            && error.code() != ERROR_IO_PENDING.to_hresult()
        {
            return Err(error);
        }
        let mut done = 0u32;
        // SAFETY: `overlapped` and the caller's buffer outlive the operation, because waiting
        // (`bwait` true) returns only once it has completed.
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

/// The Windows session this process runs in.
///
/// # Errors
/// Fails when the OS can't report it.
pub(crate) fn session_id() -> io::Result<u32> {
    let mut session = 0;
    // SAFETY: plain call; `session` is a writable u32.
    unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) }?;
    Ok(session)
}
