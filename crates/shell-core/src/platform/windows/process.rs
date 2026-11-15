//! Starting the brain.

use std::io;
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
};

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
