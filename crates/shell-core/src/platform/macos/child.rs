//! The Darwin system calls behind the shell's brain child: `posix_spawn`, an exact-pid
//! `waitpid(WNOHANG)`, SIGKILL and a read of SIGCHLD's disposition. The custody rules are the
//! shared owner's (`platform::child`); this file only makes the calls and classifies their results.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use kumochi_protocol::constants::PARENT_BOUND;

use super::pair::{self, Action, ChildEnds, ShellEnds};
use crate::platform::child::{ChildOwner, Exit, Observation, Os, SpawnWord};

/// Starts a parent-bound brain as the shell's child, with the control leg on its descriptor 3 and
/// the data leg on 4. Retirement waits the link's Quit timeout before the kill, and again after.
///
/// # Errors
/// Fails when the sockets, the path or the owner thread can't be made; nothing is left open.
pub(crate) fn start_brain(
    brain: &Path,
    word: Arc<SpawnWord>,
) -> io::Result<(ChildOwner, ShellEnds)> {
    let (shell, child) = pair::pairs()?;
    let os = Darwin::new(brain, &[OsStr::new(PARENT_BOUND)], child)?;
    Ok((
        ChildOwner::start(os, word, crate::link::QUIT_TIMEOUT)?,
        shell,
    ))
}

/// A child to start with `posix_spawn`: an absolute program, its arguments, an empty
/// environment, the two legs on descriptors 3 and 4, `/dev/null` on 0 to 2 and nothing else.
#[derive(Debug)]
pub(crate) struct Darwin {
    program: CString,
    argv: Vec<CString>,
    /// Taken by the spawn and closed when it returns, whether or not it made a child.
    ends: Option<ChildEnds>,
    pid: Option<libc::pid_t>,
}

impl Darwin {
    /// # Errors
    /// `InvalidInput` when the program isn't absolute or any string holds a NUL.
    pub(crate) fn new(program: &Path, arguments: &[&OsStr], ends: ChildEnds) -> io::Result<Self> {
        if !program.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let c_string = |value: &OsStr| {
            CString::new(value.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
        };
        let program = c_string(program.as_os_str())?;
        let argv = std::iter::once(Ok(program.clone()))
            .chain(arguments.iter().map(|argument| c_string(argument)))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            program,
            argv,
            ends: Some(ends),
            pid: None,
        })
    }
}

impl Os for Darwin {
    fn auto_reaps(&self) -> bool {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: a null new action only reads the current one into `action`, which is valid
        // for writes of one `sigaction`.
        let read = unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), action.as_mut_ptr()) };
        if read != 0 {
            return true;
        }
        // SAFETY: the zeroed value is a valid `sigaction`, and the call filled it.
        let action = unsafe { action.assume_init() };
        reaps_itself(action.sa_sigaction, action.sa_flags)
    }

    fn spawn(&mut self) -> io::Result<()> {
        let ends = self.ends.take().ok_or(io::ErrorKind::InvalidInput)?;
        let plan = pair::plan(
            ends.control.as_raw_fd(),
            ends.data.as_raw_fd(),
            descriptor_limit()?,
        )?;
        let mut actions = FileActions::new()?;
        for action in plan {
            actions.add(action)?;
        }
        let attributes = Attributes::close_by_default()?;
        let argv = null_terminated(&self.argv);
        let envp = [ptr::null_mut()];
        let mut pid = 0;
        // SAFETY: `program` and every `argv` entry are NUL-terminated strings that outlive the
        // call; `argv` and `envp` end with a null pointer; the file actions and attributes were
        // initialized and stay alive until they drop after the call.
        let error = unsafe {
            libc::posix_spawn(
                &raw mut pid,
                self.program.as_ptr(),
                &raw const actions.0,
                &raw const attributes.0,
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        self.pid = Some(pid);
        Ok(())
    }

    fn poll(&mut self) -> Observation {
        let Some(pid) = self.pid else {
            return Observation::Lost;
        };
        loop {
            let mut status = 0;
            // SAFETY: `status` is valid for one write; WNOHANG never blocks.
            let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if let Some(observation) = classify(waited, status, errno) {
                return observation;
            }
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        let pid = self.pid.ok_or(io::ErrorKind::NotFound)?;
        // SAFETY: a plain system call on our own unreaped child's pid.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Spawn file actions, destroyed on drop.
struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = ptr::null_mut();
        // SAFETY: `actions` is valid for one write of the opaque handle.
        check(unsafe { libc::posix_spawn_file_actions_init(&raw mut actions) })?;
        Ok(Self(actions))
    }

    fn add(&mut self, action: Action) -> io::Result<()> {
        let actions = &raw mut self.0;
        check(match action {
            // SAFETY: `actions` was initialized by `new`.
            Action::Dup2 { from, to } => unsafe {
                libc::posix_spawn_file_actions_adddup2(actions, from, to)
            },
            // SAFETY: as above.
            Action::Close(fd) => unsafe { libc::posix_spawn_file_actions_addclose(actions, fd) },
            Action::Null { fd, write } => {
                let flags = if write {
                    libc::O_WRONLY
                } else {
                    libc::O_RDONLY
                };
                // SAFETY: as above; the path is a static NUL-terminated string.
                unsafe {
                    libc::posix_spawn_file_actions_addopen(
                        actions,
                        fd,
                        c"/dev/null".as_ptr(),
                        flags,
                        0,
                    )
                }
            }
        })
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: initialized by `new` and destroyed once.
        unsafe { libc::posix_spawn_file_actions_destroy(&raw mut self.0) };
    }
}

/// Spawn attributes, destroyed on drop.
struct Attributes(libc::posix_spawnattr_t);

impl Attributes {
    /// Every descriptor not named by a file action is closed in the child.
    fn close_by_default() -> io::Result<Self> {
        let mut attributes = ptr::null_mut();
        // SAFETY: `attributes` is valid for one write of the opaque handle.
        check(unsafe { libc::posix_spawnattr_init(&raw mut attributes) })?;
        let mut attributes = Self(attributes);
        let flags = libc::c_short::try_from(libc::POSIX_SPAWN_CLOEXEC_DEFAULT)
            .expect("the spawn flag fits a c_short");
        // SAFETY: initialized just above.
        check(unsafe { libc::posix_spawnattr_setflags(&raw mut attributes.0, flags) })?;
        Ok(attributes)
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized by `close_by_default` and destroyed once.
        unsafe { libc::posix_spawnattr_destroy(&raw mut self.0) };
    }
}

fn check(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// The soft limit on open descriptors.
fn descriptor_limit() -> io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is valid for one write.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(limit.rlim_cur)
}

const SOCKET_FLAG_LEN: libc::socklen_t = 4;

/// Sets `SO_NOSIGPIPE` on a socket.
pub(super) fn no_sigpipe(fd: RawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: `on` is a valid c_int of the stated length for the duration of the call.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const on).cast(),
            SOCKET_FLAG_LEN,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn has_no_sigpipe(fd: RawFd) -> bool {
    let mut value: libc::c_int = 0;
    let mut len = SOCKET_FLAG_LEN;
    // SAFETY: `value` and `len` are valid for writes of the stated length.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    read == 0 && value != 0
}

/// Classifies one `waitpid(pid, WNOHANG)` result; None means interrupted, so wait again.
/// Only an exit or a signal is a reap: Darwin can report a traced stop without `WUNTRACED`.
fn classify(waited: libc::pid_t, status: libc::c_int, errno: libc::c_int) -> Option<Observation> {
    Some(match waited {
        0 => Observation::Running,
        -1 if errno == libc::EINTR => return None,
        ..0 => Observation::Lost,
        _ if libc::WIFEXITED(status) => Observation::Reaped(Exit::Code(libc::WEXITSTATUS(status))),
        _ if libc::WIFSIGNALED(status) => Observation::Reaped(Exit::Signal(libc::WTERMSIG(status))),
        _ => Observation::Stopped,
    })
}

/// SIGCHLD set to ignore, or `SA_NOCLDWAIT`, makes the kernel reap children on its own.
fn reaps_itself(handler: libc::sighandler_t, flags: libc::c_int) -> bool {
    handler == libc::SIG_IGN || flags & libc::SA_NOCLDWAIT != 0
}

fn null_terminated(strings: &[CString]) -> Vec<*mut libc::c_char> {
    strings
        .iter()
        .map(|string| string.as_ptr().cast_mut())
        .chain(std::iter::once(ptr::null_mut()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::fd::{FromRawFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::platform::child::{Refusal, Report, Settled};

    const BOUND: Duration = Duration::from_secs(10);

    fn ends() -> ChildEnds {
        pair::pairs().expect("pairs").1
    }

    fn sh(script: &str) -> Darwin {
        Darwin::new(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(script)],
            ends(),
        )
        .expect("launch")
    }

    fn read_to_eof(mut leg: &std::os::unix::net::UnixStream) -> Vec<u8> {
        match leg.set_read_timeout(Some(BOUND)) {
            // Darwin refuses the option once the peer has closed; then no read can block.
            Ok(()) | Err(_) if leg.peer_addr().is_err() => {}
            result => result.expect("timeout"),
        }
        let mut bytes = Vec::new();
        leg.read_to_end(&mut bytes).expect("EOF within the bound");
        bytes
    }

    fn next(owner: &ChildOwner) -> Report {
        owner
            .next(Instant::now() + BOUND)
            .expect("a report within the bound")
    }

    #[test]
    fn only_an_exit_or_a_signal_is_a_reap() {
        let pid = 42;
        let stop = (libc::SIGSTOP << 8) | 0o177;
        let continued = (0x13 << 8) | 0o177;
        let cases = [
            ((0, 0, 0), Some(Observation::Running)),
            ((pid, 0, 0), Some(Observation::Reaped(Exit::Code(0)))),
            ((pid, 3 << 8, 0), Some(Observation::Reaped(Exit::Code(3)))),
            (
                (pid, libc::SIGKILL, 0),
                Some(Observation::Reaped(Exit::Signal(9))),
            ),
            ((pid, stop, 0), Some(Observation::Stopped)),
            ((pid, continued, 0), Some(Observation::Stopped)),
            ((pid, 0o177, 0), Some(Observation::Stopped)),
            ((-1, 0, libc::EINTR), None),
            ((-1, 0, libc::ECHILD), Some(Observation::Lost)),
            ((-1, 0, libc::EINVAL), Some(Observation::Lost)),
        ];
        for ((waited, status, errno), expected) in cases {
            assert_eq!(
                classify(waited, status, errno),
                expected,
                "waitpid {waited}, status {status:#x}, errno {errno}"
            );
        }
    }

    #[test]
    fn an_ignored_sigchld_or_nocldwait_reaps_children_itself() {
        assert!(
            !reaps_itself(libc::SIG_DFL, 0),
            "the default keeps statuses"
        );
        assert!(reaps_itself(libc::SIG_IGN, 0), "SIG_IGN reaps");
        assert!(
            reaps_itself(libc::SIG_DFL, libc::SA_NOCLDWAIT),
            "SA_NOCLDWAIT reaps"
        );
        assert!(
            !reaps_itself(1234, libc::SA_RESTART),
            "a handler keeps statuses"
        );
        assert!(
            !sh("exit 0").auto_reaps(),
            "the test process keeps statuses"
        );
    }

    #[test]
    fn a_relative_program_or_a_nul_is_refused_before_any_spawn() {
        let relative = Darwin::new(Path::new("bin/sh"), &[], ends()).unwrap_err();
        assert_eq!(relative.kind(), io::ErrorKind::InvalidInput);
        let nul = Darwin::new(Path::new("/bin/sh"), &[OsStr::new("a\0b")], ends()).unwrap_err();
        assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure_and_frees_the_claim() {
        let word = Arc::new(SpawnWord::default());
        let missing = Darwin::new(Path::new("/nonexistent/kumochid"), &[], ends()).expect("launch");
        let owner = ChildOwner::start(missing, Arc::clone(&word), BOUND).expect("owner");
        assert!(matches!(
            next(&owner),
            Report::NotSpawned(Refusal::Spawn(error)) if error.kind() == io::ErrorKind::NotFound
        ));
        let owner = ChildOwner::start(sh("exit 0"), word, BOUND).expect("owner");
        assert!(matches!(next(&owner), Report::Spawned));
    }

    #[test]
    fn a_real_child_is_reaped_with_its_exit_code() {
        let owner =
            ChildOwner::start(sh("exit 3"), Arc::new(SpawnWord::default()), BOUND).expect("owner");
        assert!(matches!(next(&owner), Report::Spawned));
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(3)))
        ));
    }

    #[test]
    fn a_child_inherits_no_environment() {
        // Exits 1 when it was given any variable, so nothing is printed.
        let count = Darwin::new(
            Path::new("/usr/bin/awk"),
            &[OsStr::new("BEGIN { for (name in ENVIRON) exit 1; exit 0 }")],
            ends(),
        )
        .expect("launch");
        let owner = ChildOwner::start(count, Arc::new(SpawnWord::default()), BOUND).expect("owner");
        assert!(matches!(next(&owner), Report::Spawned));
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(0)))
        ));
    }

    #[test]
    fn a_stopped_real_child_stays_alive_until_killed() {
        let mut child = sh("kill -STOP $$; exit 0");
        child.spawn().expect("spawn");
        let pid = child.pid.expect("pid");
        let stopped = stopped_within(pid, Instant::now() + BOUND);
        let before_kill = child.poll();
        child.kill().expect("SIGKILL");
        let by = Instant::now() + BOUND;
        let observed = loop {
            match child.poll() {
                Observation::Running if Instant::now() < by => {}
                observed => break observed,
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(stopped, "the child stopped within the bound");
        assert_eq!(before_kill, Observation::Running, "a stop isn't a reap");
        assert_eq!(observed, Observation::Reaped(Exit::Signal(9)));
    }

    /// Watches for a stop without consuming it, so the adapter's own wait still sees the child.
    fn stopped_within(pid: libc::pid_t, by: Instant) -> bool {
        let id = libc::id_t::try_from(pid).expect("a child pid is positive");
        while Instant::now() < by {
            // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let options = libc::WSTOPPED | libc::WNOWAIT | libc::WNOHANG;
            // SAFETY: `info` is valid for one write; WNOWAIT leaves the status for the owner.
            let seen = unsafe { libc::waitid(libc::P_PID, id, &raw mut info, options) };
            if seen == 0 && info.si_pid == pid {
                return true;
            }
            if seen != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_child_reaped_by_someone_else_is_lost() {
        let mut child = sh("exit 0");
        child.spawn().expect("spawn");
        let pid = child.pid.expect("pid");
        let mut status = 0;
        // SAFETY: `status` is valid for one write; this test plays the foreign reaper.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        assert_eq!(reaped, pid, "the foreign wait reaped it");
        assert_eq!(child.poll(), Observation::Lost);
    }

    #[test]
    fn the_legs_reach_the_child_on_3_and_4_and_nothing_else_is_inherited() {
        // An inheritable descriptor well above 4, so it can't be one the child is meant to have.
        // SAFETY: duplicating standard error is a plain call; F_DUPFD leaves the copy inheritable.
        let raw = unsafe { libc::fcntl(2, libc::F_DUPFD, 64) };
        assert!(raw >= 64, "a foreign descriptor");
        // SAFETY: fcntl just returned this descriptor, and nothing else owns it.
        let foreign = unsafe { OwnedFd::from_raw_fd(raw) };
        let (shell, child) = pair::pairs().expect("pairs");
        let script = format!("printf c >&3; printf d >&4; [ -e /dev/fd/{raw} ] && exit 7; exit 0");
        let os = Darwin::new(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(&script)],
            child,
        )
        .expect("launch");
        let owner = ChildOwner::start(os, Arc::new(SpawnWord::default()), BOUND).expect("owner");
        assert!(matches!(next(&owner), Report::Spawned));
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(0)))
        ));
        // EOF also shows the shell dropped its copies of the child's ends after the spawn.
        assert_eq!(read_to_eof(&shell.control), b"c");
        assert_eq!(read_to_eof(&shell.data), b"d");
        drop(foreign);
    }

    #[test]
    fn a_failed_spawn_closes_the_childs_ends() {
        let (shell, child) = pair::pairs().expect("pairs");
        let missing = Darwin::new(Path::new("/nonexistent/kumochid"), &[], child).expect("launch");
        let owner =
            ChildOwner::start(missing, Arc::new(SpawnWord::default()), BOUND).expect("owner");
        assert!(matches!(
            next(&owner),
            Report::NotSpawned(Refusal::Spawn(_))
        ));
        assert_eq!(read_to_eof(&shell.control), b"");
        assert_eq!(read_to_eof(&shell.data), b"");
    }

    #[test]
    fn start_brain_passes_exactly_one_argument() {
        // `test` exits 0 for exactly one non-empty argument, 1 for none and 2 for two.
        let test = Path::new("/bin/test");
        let (owner, shell) = start_brain(test, Arc::new(SpawnWord::default())).expect("start");
        assert!(matches!(next(&owner), Report::Spawned));
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(0)))
        ));
        assert_eq!(read_to_eof(&shell.control), b"");
    }
}
