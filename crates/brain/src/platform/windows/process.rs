//! Starting the shell outside the brain.

use std::io;
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
};

/// Starts `exe` with no console, with `env` set over this process's environment, outside the
/// brain's process group and, where the brain's own job allows it, outside that job, so the shell
/// never dies with the brain.
///
/// # Errors
/// Fails when the process can't be created.
#[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
pub fn launch_detached(exe: &Path, env: &[(&str, &str)]) -> io::Result<Child> {
    let _spawning = crate::platform::spawn_lock();
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let spawn = |flags: u32| detached_command(exe, env).creation_flags(flags).spawn();
    // A job that forbids breakaway refuses the flag; then the shell stays in that job.
    match spawn(base | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => spawn(base),
        result => result,
    }
}

/// What [`launch_detached`] starts, before its creation flags.
#[must_use]
pub fn detached_command(exe: &Path, env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .envs(env.iter().copied());
    command
}
