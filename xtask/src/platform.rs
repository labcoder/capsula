//! Win32 process discovery, counters and isolated input-session tools.
#![expect(
    unsafe_code,
    reason = "Win32 calls are confined to the platform module"
)]

use std::ffi::OsStr;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kumochi_protocol::constants::{BRAIN_EXE, SHELL_EXE};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentThread, GetExitCodeProcess, GetProcessTimes, NORMAL_PRIORITY_CLASS,
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_VM_READ,
    WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};
use windows::core::BOOL;

pub(crate) mod gate_capture;
pub(crate) mod gate_pipe;
pub(crate) mod input_session;

pub(crate) fn close_shell_window(child: &mut Child) -> Result<()> {
    unsafe extern "system" fn collect(window: HWND, param: LPARAM) -> BOOL {
        let state =
            std::ptr::with_exposed_provenance_mut::<(u32, Vec<HWND>)>(param.0.cast_unsigned());
        // SAFETY: EnumWindows synchronously passes the exclusive state pointer supplied below.
        let (shell_pid, windows) = unsafe { &mut *state };
        let mut pid = 0;
        // SAFETY: window is supplied by EnumWindows and pid is writable.
        unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
        if pid == *shell_pid {
            // SAFETY: checking a window's visibility requires no caller-owned pointers.
            if unsafe { IsWindowVisible(window) }.as_bool() {
                windows.push(window);
            }
        }
        BOOL::from(true)
    }
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Ok(proxy) = input_session::owned_window(child, input_session::WindowKind::ProductProxy) {
        return proxy.close();
    }
    let mut state = (child.id(), Vec::<HWND>::new());
    // SAFETY: collect receives the live exclusive state pointer for this synchronous enumeration.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM((&raw mut state).expose_provenance().cast_signed()),
        )
    }?;
    anyhow::ensure!(
        state.1.len() == 1,
        "expected one visible baseline shell window; found {}",
        state.1.len()
    );
    let window = state.1[0];
    let mut pid = 0;
    // SAFETY: window came from EnumWindows and pid is writable.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    anyhow::ensure!(
        pid == child.id(),
        "baseline window ownership changed before close"
    );
    // SAFETY: the window belongs to the spawned shell; WM_CLOSE has no pointer parameters.
    unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }?;
    Ok(())
}

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

pub(crate) fn launch(
    exe: &Path,
    quit_after_ms: u64,
    extra: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Child> {
    Ok(Command::new(exe)
        .args(extra)
        .arg(format!("--quit-after-ms={quit_after_ms}"))
        .creation_flags(NORMAL_PRIORITY_CLASS.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

/// The command that force-ends `pid` and every process it started. std can end one child, not
/// its descendants (cargo's rustc, a run's brain), so this is the platform's own tool.
pub(crate) fn kill_tree(pid: u32) -> Result<Command> {
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let mut command = Command::new(Path::new(&system_root).join("System32/taskkill.exe"));
    command
        .args(["/T", "/F", "/PID"])
        .arg(pid.to_string())
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub(crate) fn capture_dwm(samples: u64, path: &Path) -> Result<Child> {
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    Ok(
        Command::new(Path::new(&system_root).join("System32/typeperf.exe"))
            .args([
                r"\Process(dwm)\% Processor Time",
                r"\Process(dwm)\ID Process",
                "-si",
                "1",
                "-sc",
            ])
            .arg(samples.to_string())
            .arg("-o")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW.0 | NORMAL_PRIORITY_CLASS.0)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(path.with_extension("stdout.txt"))?)
            .stderr(std::fs::File::create(path.with_extension("stderr.txt"))?)
            .spawn()?,
    )
}

pub(crate) fn thread_cycles() -> Result<u64> {
    // SAFETY: the pseudo-handle refers to this live thread and must not be closed.
    let thread = unsafe { GetCurrentThread() };
    let mut cycles = 0;
    // SAFETY: the current-thread handle has query access and cycles is writable.
    unsafe { QueryThreadCycleTime(thread, &raw mut cycles) }?;
    Ok(cycles)
}

#[derive(Debug)]
pub(crate) struct Process {
    pub(crate) pid: u32,
    handle: OwnedHandle,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sample {
    pub(crate) cpu: Duration,
    pub(crate) cycles: u64,
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
        let mut cycles = 0;
        // SAFETY: the live process handle has query access and cycles is writable.
        unsafe { QueryProcessCycleTime(handle, &raw mut cycles) }?;
        if let Some(code) = self.exit_code()? {
            bail!("pid {} exited during sampling with code {code}", self.pid);
        }
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Ok(Sample {
            cpu: Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
            cycles,
            private_bytes: memory.PrivateUsage,
        })
    }
}
