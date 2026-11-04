//! Starting helpers: an explicit argument vector, environment, working folder and standard
//! handles, inside the brain's jobs from the first instruction. std's `Command` can't place a
//! process in a job before it runs (`raw_attribute` is unstable) and inherits the environment
//! unless told otherwise, so this calls `CreateProcessW` directly. macOS counterpart:
//! `posix_spawn` with its own process group and an explicit `envp`, ended with `killpg`
//! (unverified).

use std::ffi::{OsStr, OsString, c_void};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, PipeReader, PipeWriter};
use std::iter;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use windows_sys::Win32::Foundation::{
    FALSE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, TRUE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW,
};

use super::job::{AttributeList, Helper, Job, size_u32};

/// The longest command line `CreateProcessW` takes, in UTF-16 units including the NUL.
pub(crate) const MAX_COMMAND_LINE: usize = 32_767;

const NUL: u16 = 0;
const QUOTE: u16 = 0x22;
const BACKSLASH: u16 = 0x5C;
const SPACE: u16 = 0x20;
const TAB: u16 = 0x09;
const EQUALS: u16 = 0x3D;

/// Held from the moment a child's handle ends become inheritable until they're closed, and
/// around every other process creation in the brain, so no other child inherits them.
static SPAWNING: Mutex<()> = Mutex::new(());

/// Takes the process-creation lock. Any code in the brain that starts a process with std's
/// `Command` holds it across the spawn, because std passes `bInheritHandles = TRUE`.
pub(crate) fn lock_spawns() -> MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
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

/// The child's ends of its standard handles: each distinct handle once, and which one is which.
struct ChildEnds {
    handles: Vec<OwnedHandle>,
    input: HANDLE,
    output: HANDLE,
    error: HANDLE,
}

/// Starts `launch` with only its own standard handles inheritable and nothing else from the
/// brain: no environment variable, no working folder and no other handle.
///
/// # Errors
/// `InvalidInput` for no job, a command line over [`MAX_COMMAND_LINE`], an environment name that's
/// empty, holds `=` or NUL, or repeats another without regard to case, or a value holding NUL;
/// otherwise the OS's error creating the pipes or the process.
pub fn spawn(launch: &Launch<'_>) -> io::Result<Spawned> {
    let mut command_line = command_line(launch.exe, launch.args)?;
    let environment = environment_block(launch.env)?;
    let application = wide(launch.exe.as_os_str());
    let cwd = wide(launch.cwd.as_os_str());
    if launch.jobs.is_empty() {
        return Err(invalid("a helper starts inside a job"));
    }
    let jobs: Vec<HANDLE> = launch.jobs.iter().map(|job| job.raw()).collect();
    let (child, stdin, stdout, stderr) = standard_handles(launch.stdio)?;
    let inherited: Vec<HANDLE> = child
        .iter()
        .flat_map(|ends| ends.handles.iter().map(AsRawHandle::as_raw_handle))
        .collect();
    let attributes = AttributeList::with_jobs(&jobs, &inherited)?;
    let mut startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_u32::<STARTUPINFOEXW>(),
            ..Default::default()
        },
        lpAttributeList: attributes.0.as_ptr().cast_mut().cast(),
    };
    if let Some(ends) = &child {
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = ends.input;
        startup.StartupInfo.hStdOutput = ends.output;
        startup.StartupInfo.hStdError = ends.error;
    }
    let window = if launch.stdio == Stdio::Console {
        CREATE_NEW_CONSOLE
    } else {
        CREATE_NO_WINDOW
    };
    let mut info = PROCESS_INFORMATION::default();
    let created = {
        let _spawning = lock_spawns();
        let created = inherited
            .iter()
            .try_for_each(|&handle| set_inheritable(handle))
            .and_then(|()| {
                // SAFETY: the application, command line, environment and folder are
                // NUL-terminated and the command line is writable, as CreateProcessW requires;
                // the attribute list and the handles it names outlive the call; only the handles
                // in that list are inherited.
                let ok = unsafe {
                    CreateProcessW(
                        application.as_ptr(),
                        command_line.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        if inherited.is_empty() { FALSE } else { TRUE },
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | window,
                        environment.as_ptr().cast::<c_void>(),
                        cwd.as_ptr(),
                        (&raw const startup).cast(),
                        &raw mut info,
                    )
                };
                if ok == FALSE {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        // The child's ends close while the lock is still held, so nothing else inherits them.
        drop(child);
        created
    };
    created?;
    // SAFETY: CreateProcessW returned both handles, owned only here; the thread one closes now.
    drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
    // SAFETY: as above.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    Ok(Spawned {
        process: Helper::new(process, info.dwProcessId),
        stdin,
        stdout,
        stderr,
    })
}

/// The child's ends of its standard handles and the brain's ends of its pipes.
type StandardHandles = (
    Option<ChildEnds>,
    Option<PipeWriter>,
    Option<PipeReader>,
    Option<PipeReader>,
);

fn standard_handles(kind: Stdio) -> io::Result<StandardHandles> {
    Ok(match kind {
        Stdio::Piped => {
            let (child_in, stdin) = io::pipe()?;
            let (stdout, child_out) = io::pipe()?;
            let (stderr, child_err) = io::pipe()?;
            let handles: Vec<OwnedHandle> =
                vec![child_in.into(), child_out.into(), child_err.into()];
            let ends = ChildEnds {
                input: handles[0].as_raw_handle(),
                output: handles[1].as_raw_handle(),
                error: handles[2].as_raw_handle(),
                handles,
            };
            (Some(ends), Some(stdin), Some(stdout), Some(stderr))
        }
        Stdio::Null => {
            let null: OwnedHandle = OpenOptions::new()
                .read(true)
                .write(true)
                .open("NUL")?
                .into();
            let raw = null.as_raw_handle();
            let ends = ChildEnds {
                handles: vec![null],
                input: raw,
                output: raw,
                error: raw,
            };
            (Some(ends), None, None, None)
        }
        Stdio::Console => (None, None, None, None),
    })
}

fn set_inheritable(handle: HANDLE) -> io::Result<()> {
    // SAFETY: `handle` is one of the child ends this call still owns.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(iter::once(NUL)).collect()
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

/// `"exe" args...` with its NUL, quoted by the rules `CommandLineToArgvW` reads.
pub(crate) fn command_line(exe: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut line = vec![QUOTE];
    for unit in exe.as_os_str().encode_wide() {
        // argv[0] is read up to the next quote with no escapes, so a quote can't be carried.
        if unit == QUOTE {
            return Err(invalid("executable path holds a quote"));
        }
        line.push(unit);
    }
    line.push(QUOTE);
    for arg in args {
        line.push(SPACE);
        push_quoted(&mut line, arg);
    }
    line.push(NUL);
    if line.len() > MAX_COMMAND_LINE {
        return Err(invalid("command line too long"));
    }
    Ok(line)
}

fn push_quoted(line: &mut Vec<u16>, arg: &OsStr) {
    let units: Vec<u16> = arg.encode_wide().collect();
    if !units.is_empty() && !units.iter().any(|&u| u == SPACE || u == TAB || u == QUOTE) {
        line.extend(units);
        return;
    }
    line.push(QUOTE);
    let mut backslashes = 0;
    for unit in units {
        match unit {
            BACKSLASH => backslashes += 1,
            QUOTE => {
                line.extend(iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
                line.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                line.extend(iter::repeat_n(BACKSLASH, backslashes));
                line.push(unit);
                backslashes = 0;
            }
        }
    }
    // Doubled so the closing quote isn't escaped.
    line.extend(iter::repeat_n(BACKSLASH, backslashes * 2));
    line.push(QUOTE);
}

/// A `CREATE_UNICODE_ENVIRONMENT` block: `NAME=value` entries sorted by ordinal uppercase,
/// each NUL-terminated, then one more NUL. An empty set is two NULs, never a null pointer, which
/// would inherit the brain's environment.
pub(crate) fn environment_block(env: &[(OsString, OsString)]) -> io::Result<Vec<u16>> {
    let mut vars = Vec::with_capacity(env.len());
    for (name, value) in env {
        let name: Vec<u16> = name.encode_wide().collect();
        let value: Vec<u16> = value.encode_wide().collect();
        if name.is_empty() || name.contains(&EQUALS) || name.contains(&NUL) {
            return Err(invalid("bad environment name"));
        }
        if value.contains(&NUL) {
            return Err(invalid("environment value holds NUL"));
        }
        let folded: Vec<u16> = name.iter().map(|&unit| upper(unit)).collect();
        vars.push((folded, name, value));
    }
    vars.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    if vars.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid("environment name repeated"));
    }
    let mut block = Vec::new();
    for (_, name, value) in vars {
        block.extend(name);
        block.push(EQUALS);
        block.extend(value);
        block.push(NUL);
    }
    if block.is_empty() {
        block.push(NUL);
    }
    block.push(NUL);
    Ok(block)
}

/// One UTF-16 unit in uppercase where it maps to exactly one unit, as Windows compares names.
fn upper(unit: u16) -> u16 {
    char::from_u32(u32::from(unit))
        .and_then(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(u), None) => u16::try_from(u32::from(u)).ok(),
                _ => None,
            }
        })
        .unwrap_or(unit)
}

#[cfg(test)]
mod tests;
