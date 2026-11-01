//! The shell's single-instance lock and starting the brain.

use std::io;
use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateMutexW, DETACHED_PROCESS,
};
use windows::core::HSTRING;

/// Held for the shell's whole life; another shell in this session sees it and exits.
#[derive(Debug)]
pub(crate) struct InstanceLock(
    #[expect(dead_code, reason = "held only to keep the mutex open")] OwnedHandle,
);

/// Takes the named mutex `name`, or returns `None` when another process already holds it.
///
/// # Errors
/// Fails when the OS refuses to create or open the mutex.
pub(crate) fn lock_single_instance(name: &str) -> io::Result<Option<InstanceLock>> {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call; default security, not inherited.
    let handle = unsafe { CreateMutexW(None, false, &wide) }?;
    let existed =
        io::Error::last_os_error().raw_os_error() == Some(win32_code(ERROR_ALREADY_EXISTS.0));
    // SAFETY: CreateMutexW returned a valid handle that nothing else owns.
    let lock = InstanceLock(unsafe { OwnedHandle::from_raw_handle(handle.0) });
    Ok((!existed).then_some(lock))
}

/// Starts `exe` with no console, outside the shell's process group, and outside any job the
/// shell is in where that job allows it, so the brain outlives whatever started the shell.
///
/// # Errors
/// Fails when the process can't be created.
pub(crate) fn launch_detached(exe: &Path) -> io::Result<()> {
    let base = DETACHED_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0;
    let spawn = |flags: u32| {
        Command::new(exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // A job that forbids breakaway refuses the flag; then the brain stays in that job.
    let child = match spawn(base | CREATE_BREAKAWAY_FROM_JOB.0) {
        Err(e) if e.raw_os_error() == Some(win32_code(ERROR_ACCESS_DENIED.0)) => spawn(base),
        result => result,
    }?;
    tracing::info!(pid = child.id(), "started the brain");
    Ok(())
}

fn win32_code(code: u32) -> i32 {
    i32::try_from(code).expect("Win32 error codes fit in i32")
}
