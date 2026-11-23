//! No launch can produce a contained process without a native adapter.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

use super::{ChildPipes, Launch, Spawned};

/// Owns native process containment. No value exists without a containment adapter.
#[derive(Debug)]
pub struct Job(Infallible);

impl Job {
    /// Creates process containment.
    ///
    /// # Errors
    /// Returns `Unsupported` without starting anything.
    pub fn new() -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn is_shut_down(&self) -> bool {
        match self.0 {}
    }

    pub(crate) fn shut_down(&self) -> io::Result<()> {
        match self.0 {}
    }

    /// Ends the contained processes and waits for them to exit.
    ///
    /// # Errors
    /// On platforms where a job exists, returns an error if termination can't be confirmed.
    pub fn terminate(&self) -> io::Result<()> {
        match self.0 {}
    }

    /// Starts a helper with piped input and output in the supplied jobs.
    ///
    /// # Errors
    /// Returns `Unsupported` before creating a process or pipe.
    pub fn spawn_piped(_: &Path, _: &str, _: &[&Self]) -> io::Result<(Helper, ChildPipes)> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// A handle to a contained process; it cannot exist without a containment adapter.
#[derive(Debug)]
pub struct Helper(Infallible);

impl Helper {
    /// The helper's process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        match self.0 {}
    }

    /// Requests termination.
    ///
    /// # Errors
    /// On platforms with a helper adapter, fails if termination can't be requested.
    pub fn terminate(&self) -> io::Result<()> {
        match self.0 {}
    }

    /// The exit code of a process that has exited.
    ///
    /// # Errors
    /// On platforms with a helper adapter, fails if the OS can't report the code.
    pub fn exit_code(&self) -> io::Result<u32> {
        match self.0 {}
    }

    /// Whether the process exits within the deadline.
    ///
    /// # Errors
    /// On platforms with a helper adapter, fails if the wait itself fails.
    pub fn exits_within(&self, _: Duration) -> io::Result<bool> {
        match self.0 {}
    }
}

/// Starts an explicitly configured, contained process.
///
/// # Errors
/// Returns `Unsupported` before creating a process or pipe.
pub fn spawn(_: &Launch<'_>) -> io::Result<Spawned> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
fn spawn_if(launch: &Launch<'_>, _: &dyn Fn() -> Option<Duration>) -> io::Result<Spawned> {
    spawn(launch)
}

/// Starts the shell outside the brain's process group.
///
/// # Errors
/// Returns `Unsupported` without creating a process.
pub fn launch_detached(_: &Path, _: &[(&str, &str)]) -> io::Result<Child> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Returns the OS-supplied values an explicitly configured helper can inherit.
///
/// # Errors
/// Returns `Unsupported` rather than inheriting or guessing values.
pub fn system_env() -> io::Result<Vec<(OsString, OsString)>> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Stdio;

    #[test]
    fn native_starts_refuse_without_containment() {
        let path = Path::new("");
        let launch = Launch {
            exe: path,
            args: &[],
            cwd: path,
            env: &[],
            stdio: Stdio::Piped,
            jobs: &[],
        };

        assert_eq!(
            Job::new().expect_err("no containment").kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            spawn(&launch).expect_err("no child").kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            spawn_if(&launch, &|| panic!("no launch to budget"))
                .expect_err("no child")
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            Job::spawn_piped(path, "", &[])
                .expect_err("no helper or pipes")
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            launch_detached(path, &[]).expect_err("no shell").kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            system_env().expect_err("no inherited environment").kind(),
            io::ErrorKind::Unsupported
        );
    }
}
