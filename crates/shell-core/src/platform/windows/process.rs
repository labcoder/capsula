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
    let spawn = |flags: u32| command(exe).creation_flags(flags).spawn();
    // A job that forbids breakaway refuses the flag; then the brain stays in that job.
    let child = match spawn(base | CREATE_BREAKAWAY_FROM_JOB.0) {
        Err(e) if e.raw_os_error() == Some(win32_code(ERROR_ACCESS_DENIED.0)) => spawn(base),
        result => result,
    }?;
    tracing::info!(pid = child.id(), "started the brain");
    Ok(())
}

/// What [`launch_detached`] starts, before its creation flags: this shell's environment, unchanged.
fn command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn win32_code(code: u32) -> i32 {
    i32::try_from(code).expect("Win32 error codes fit in i32")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};

    use kumochi_brain::connections::model::budget::handed_on;
    use kumochi_brain::platform::detached_command;
    use kumochi_protocol::constants::{
        LIVE_BUDGET_ENV, LIVE_KEY_CATALOG_BUDGET_ENV, LIVE_KEY_MESSAGES_BUDGET_ENV,
    };

    use super::*;

    type Env = BTreeMap<OsString, OsString>;

    /// The environment of the process `command` starts from a parent whose environment is `env`.
    fn started(mut env: Env, command: &Command) -> Env {
        for (name, value) in command.get_envs() {
            match value {
                Some(value) => env.insert(name.to_owned(), value.to_owned()),
                None => env.remove(name),
            };
        }
        env
    }

    /// A brain with `env`, the shell it relaunches and the brain that shell starts.
    fn chain(env: &[(&str, &str)]) -> Env {
        let brain: Env = env
            .iter()
            .map(|&(name, value)| (name.into(), value.into()))
            .collect();
        let budgets = handed_on(|name| brain.contains_key(OsStr::new(name)));
        let shell = started(
            brain.clone(),
            &detached_command(Path::new("kumochi.exe"), budgets),
        );
        started(shell, &command(Path::new("kumochid.exe")))
    }

    fn value<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
        env.get(OsStr::new(name)).and_then(|value| value.to_str())
    }

    #[test]
    fn a_brain_started_by_a_shell_a_key_slice_brain_relaunched_gets_no_key_slice() {
        let replacement = chain(&[
            (LIVE_KEY_CATALOG_BUDGET_ENV, "2"),
            (LIVE_KEY_MESSAGES_BUDGET_ENV, "3"),
            (LIVE_BUDGET_ENV, "4"),
        ]);
        assert_eq!(value(&replacement, LIVE_KEY_CATALOG_BUDGET_ENV), Some("0"));
        assert_eq!(value(&replacement, LIVE_KEY_MESSAGES_BUDGET_ENV), Some("0"));
        assert_eq!(value(&replacement, LIVE_BUDGET_ENV), Some("4"));
        let replacement = chain(&[(LIVE_KEY_MESSAGES_BUDGET_ENV, "3")]);
        assert_eq!(value(&replacement, LIVE_KEY_CATALOG_BUDGET_ENV), Some("0"));
        assert_eq!(value(&replacement, LIVE_KEY_MESSAGES_BUDGET_ENV), Some("0"));

        let replacement = chain(&[(LIVE_BUDGET_ENV, "4")]);
        assert_eq!(
            replacement,
            Env::from([(LIVE_BUDGET_ENV.into(), "4".into())])
        );
    }
}
