//! Win32 process discovery and counters for the input-free idle benchmark.
#![expect(
    unsafe_code,
    reason = "Win32 calls are confined to the platform module"
)]

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Result, bail};
use capsula_protocol::constants::{BRAIN_EXE, SHELL_EXE};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, NORMAL_PRIORITY_CLASS, OpenProcess,
    PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_VM_READ, WaitForSingleObject,
};

#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
    pub(crate) shell: bool,
}

pub(crate) fn processes() -> Result<Vec<Entry>> {
    // SAFETY: process snapshots require no caller-owned pointers.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
    // SAFETY: the successful snapshot call transfers a unique, valid handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(size_of::<PROCESSENTRY32W>())?,
        ..Default::default()
    };
    // SAFETY: the snapshot is live and entry is writable with the documented size.
    let mut next = unsafe { Process32FirstW(HANDLE(snapshot.as_raw_handle()), &raw mut entry) };
    let mut entries = Vec::new();
    loop {
        match next {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(error) => return Err(error.into()),
        }
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
        if name.eq_ignore_ascii_case(SHELL_EXE) || name.eq_ignore_ascii_case(BRAIN_EXE) {
            entries.push(Entry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                shell: name.eq_ignore_ascii_case(SHELL_EXE),
            });
        }
        // SAFETY: the snapshot is live and entry remains writable with the documented size.
        next = unsafe { Process32NextW(HANDLE(snapshot.as_raw_handle()), &raw mut entry) };
    }
    Ok(entries)
}

pub(crate) fn launch(exe: &Path, quit_after_ms: u64) -> Result<Child> {
    Ok(Command::new(exe)
        .arg(format!("--quit-after-ms={quit_after_ms}"))
        .creation_flags(NORMAL_PRIORITY_CLASS.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

#[derive(Debug)]
pub(crate) struct Process {
    pub(crate) pid: u32,
    handle: OwnedHandle,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sample {
    pub(crate) cpu: Duration,
    pub(crate) private_bytes: usize,
}

impl Process {
    pub(crate) fn open(pid: u32) -> Result<Self> {
        // SAFETY: OpenProcess takes a numeric PID and returns a separately owned handle.
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }?;
        // SAFETY: OpenProcess succeeded and ownership of its unique handle is transferred.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        Ok(Self { pid, handle })
    }

    pub(crate) fn exit_code(&self) -> Result<Option<u32>> {
        let handle = HANDLE(self.handle.as_raw_handle());
        // SAFETY: the process handle is live, has SYNCHRONIZE access, and the timeout is zero.
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            bail!(
                "waiting for pid {}: {}",
                self.pid,
                std::io::Error::last_os_error()
            );
        }
        let mut code = 0;
        // SAFETY: the handle is live and code is a writable u32.
        unsafe { GetExitCodeProcess(handle, &raw mut code) }?;
        Ok(Some(code))
    }

    pub(crate) fn sample(&self) -> Result<Sample> {
        if let Some(code) = self.exit_code()? {
            bail!("pid {} exited before sampling with code {code}", self.pid);
        }
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let handle = HANDLE(self.handle.as_raw_handle());
        // SAFETY: the live process handle has query access; all four FILETIMEs are writable.
        unsafe {
            GetProcessTimes(
                handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        }?;
        let mut memory = PROCESS_MEMORY_COUNTERS_EX {
            cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>())?,
            ..Default::default()
        };
        // SAFETY: EX begins with PROCESS_MEMORY_COUNTERS and cb specifies the full writable allocation.
        unsafe { GetProcessMemoryInfo(handle, (&raw mut memory).cast(), memory.cb) }?;
        if let Some(code) = self.exit_code()? {
            bail!("pid {} exited during sampling with code {code}", self.pid);
        }
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Ok(Sample {
            cpu: Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
            private_bytes: memory.PrivateUsage,
        })
    }
}
