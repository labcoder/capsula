//! The launch contract shared by the native and unavailable process adapters.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{PipeReader, PipeWriter};
use std::path::Path;

use super::Mode;

#[cfg(not(windows))]
mod unsupported;

#[cfg(windows)]
pub(crate) use super::windows::spawn_if;
#[cfg(windows)]
pub use super::windows::{Helper, Job, launch_detached, spawn, system_env};
#[cfg(not(windows))]
pub(crate) use unsupported::spawn_if;
#[cfg(not(windows))]
pub use unsupported::{Helper, Job, launch_detached, spawn, system_env};

/// The containment guarantees of a configured process adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainmentTier {
    /// Windows Job membership and kill on close.
    Kernel,
    /// A private process group and the brain's original-parent watch.
    Group,
}

/// Availability is not permission to launch or proof of custody of a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Containment {
    /// There is no adapter for this launch mode.
    Unsupported,
    /// The fixed shell-to-brain adapter exists, but product runtime admission is off.
    Staged(ContainmentTier),
    /// This mode can attempt to acquire the tier's native custody.
    RuntimeEnabled(ContainmentTier),
}

/// Reports the adapter for a launch mode, without changing runtime admission.
#[must_use]
pub const fn containment(mode: Mode) -> Containment {
    match mode {
        Mode::Detached if cfg!(windows) => Containment::RuntimeEnabled(ContainmentTier::Kernel),
        Mode::ParentBound if cfg!(target_os = "macos") => {
            Containment::Staged(ContainmentTier::Group)
        }
        _ => Containment::Unsupported,
    }
}

/// Exercises the real parent watch in the shell's isolated headless child fixture.
///
/// # Errors
/// Returns an error if the watch cannot arm, the ready notice fails, or confirmed parent loss
/// fails to close the admission gate before the deadline.
#[cfg(all(target_os = "macos", debug_assertions, feature = "test-support"))]
pub fn group_parent_fixture(
    by: std::time::Instant,
    ready: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io;

    use super::ShellProcess;
    use super::shell_process::Verdict;

    let shell = ShellProcess::arm_parent().map_err(io::Error::other)?;
    shell
        .fence()
        .map_err(|_| io::Error::other("watch not armed"))?;
    let mut loss = shell
        .loss()
        .ok_or_else(|| io::Error::other("missing parent watch"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    ready()?;
    let verdict = runtime
        .block_on(async { tokio::time::timeout_at(by.into(), Verdict::wait(&mut loss)).await })?;
    if verdict != Verdict::Exited
        || shell.fence() != Err(Verdict::Exited)
        || shell.claim_commit().is_some()
    {
        return Err(io::Error::other("parent loss did not close admission"));
    }
    Ok(())
}

/// The parent's non-inheritable ends of a helper's stdin and stdout pipes.
#[derive(Debug)]
pub struct ChildPipes {
    /// Writes to the child's stdin.
    pub stdin: File,
    /// Reads the child's stdout.
    pub stdout: File,
}

/// What to start and how.
pub struct Launch<'a> {
    /// The absolute path of the executable; it's also `argv[0]`.
    pub exe: &'a Path,
    /// The arguments after `argv[0]`, quoted so `CommandLineToArgvW` gives them back exactly.
    pub args: &'a [OsString],
    /// The child's working folder.
    pub cwd: &'a Path,
    /// The child's whole environment; nothing is inherited.
    pub env: &'a [(OsString, OsString)],
    /// Its standard handles.
    pub stdio: Stdio,
    /// The jobs it starts in; later jobs nest in earlier ones.
    pub jobs: &'a [&'a Job],
}

impl fmt::Debug for Launch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts only: argument and environment values stay out of logs.
        f.debug_struct("Launch")
            .field("exe", &self.exe)
            .field("args", &self.args.len())
            .field("env", &self.env.len())
            .field("stdio", &self.stdio)
            .field("jobs", &self.jobs.len())
            .finish_non_exhaustive()
    }
}

/// A child's standard handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdio {
    /// Three anonymous pipes to the brain; no window.
    Piped,
    /// The `NUL` device for all three; no window.
    Null,
    /// A new visible console with its own handles, for `claude auth login` only, so a prompt or
    /// a fallback URL is answerable. The brain sees none of it.
    Console,
}

/// A started child and the brain's ends of its pipes, for [`Stdio::Piped`].
pub struct Spawned {
    /// The process.
    pub process: Helper,
    /// Writes to its stdin.
    pub stdin: Option<PipeWriter>,
    /// Reads its stdout.
    pub stdout: Option<PipeReader>,
    /// Reads its stderr.
    pub stderr: Option<PipeReader>,
}

impl fmt::Debug for Spawned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spawned")
            .field("pid", &self.process.pid())
            .field("stdin", &self.stdin.is_some())
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Containment, ContainmentTier, Mode, containment};

    #[test]
    fn only_the_detached_kernel_tier_can_attempt_runtime_admission() {
        use crate::platform::supports;

        let (detached, parent_bound) = if cfg!(windows) {
            (
                Containment::RuntimeEnabled(ContainmentTier::Kernel),
                Containment::Unsupported,
            )
        } else if cfg!(target_os = "macos") {
            (
                Containment::Unsupported,
                Containment::Staged(ContainmentTier::Group),
            )
        } else {
            (Containment::Unsupported, Containment::Unsupported)
        };
        assert_eq!(containment(Mode::Detached), detached);
        assert_eq!(containment(Mode::ParentBound), parent_bound);
        assert_eq!(supports(Mode::Detached), cfg!(windows));
        assert!(!supports(Mode::ParentBound));
    }
}
