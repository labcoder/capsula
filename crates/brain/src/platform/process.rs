//! The launch contract shared by the native and unavailable process adapters.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{PipeReader, PipeWriter};
use std::path::Path;

#[cfg(not(windows))]
mod unsupported;

#[cfg(windows)]
pub(crate) use super::windows::spawn_if;
#[cfg(windows)]
pub use super::windows::{Helper, Job, launch_detached, spawn, system_env};
#[cfg(not(windows))]
pub use unsupported::{Helper, Job, launch_detached, spawn, system_env};

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
