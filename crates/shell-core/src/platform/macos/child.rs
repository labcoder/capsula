//! The Darwin system calls behind the shell's brain child: `posix_spawn`, an exact-pid
//! `waitpid(WNOHANG)` and a read of SIGCHLD's disposition. The custody rules are the
//! shared owner's (`platform::child`); this file only makes the calls and classifies their results.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use kumochi_protocol::constants::PARENT_BOUND;

use super::pair::{self, Action, ChildEnds, ShellEnds};
use crate::platform::child::{ChildOwner, Exit, Observation, Os, Refusal, Report, SpawnWord};
use crate::platform::unix::brain_group::{OwnedGroup, RetirementError, SET_SESSION};

/// How long `start_brain` waits for the spawn report. A spawn takes milliseconds; this only
/// bounds one that stalls.
const SPAWN_REPORT: Duration = Duration::from_secs(5);

/// Why `start_brain` handed out no ends. All four are closed by the time it returns.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotStarted {
    /// No child exists, and this attempt starts none.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// The spawn didn't report, by its deadline or before its owner thread ended, so a child may
    /// exist or still start. Its owner is dropped, so it retires any child it makes, and the
    /// spawn claim stays taken until that child is reaped.
    #[error("the brain's spawn didn't report")]
    Pending,
}

/// Starts a parent-bound brain as the shell's child, with the control leg on its descriptor 3 and
/// the data leg on 4, and hands out the shell's ends only once the child exists. Retirement
/// waits the link's Quit timeout before the kill, and again after. Blocks until the owner reports
/// its one spawn attempt, until `by` or for `SPAWN_REPORT`, whichever ends first. Call it off the
/// UI thread.
///
/// # Errors
/// `Pending` when no report came in time or the owner thread ended without one. Otherwise the
/// owner's refusal, or `Spawn` when the sockets, the path or the owner thread can't be made.
pub(crate) fn start_brain(
    brain: &Path,
    word: Arc<SpawnWord>,
    by: Instant,
) -> Result<(ChildOwner, ShellEnds), NotStarted> {
    let (shell, child) = pair::pairs().map_err(Refusal::Spawn)?;
    let os = Darwin::new(brain, &[OsStr::new(PARENT_BOUND)], child).map_err(Refusal::Spawn)?;
    Ok((reported(os, word, by)?, shell))
}

/// Starts the owner and waits for its spawn report, until `by` or for `SPAWN_REPORT` at most.
fn reported(os: impl Os, word: Arc<SpawnWord>, by: Instant) -> Result<ChildOwner, NotStarted> {
    let owner = ChildOwner::start(os, word, crate::link::QUIT_TIMEOUT).map_err(Refusal::Spawn)?;
    started(owner.spawned(by.min(Instant::now() + SPAWN_REPORT))).map(|()| owner)
}

/// What the spawn report proves. Only a report says whether a child exists; a thread that ended
/// without one proves nothing, so it's `Pending` like a late report.
fn started(report: Result<Report, RecvTimeoutError>) -> Result<(), NotStarted> {
    match report {
        Ok(Report::Spawned) => Ok(()),
        Ok(Report::NotSpawned(refusal)) => Err(refusal.into()),
        Ok(Report::Settled(_))
        | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            Err(NotStarted::Pending)
        }
    }
}

/// A child to start with `posix_spawn`: an absolute program, its arguments, an empty
/// environment, the two legs on descriptors 3 and 4, `/dev/null` on 0 to 2 and nothing else.
#[derive(Debug)]
pub(crate) struct Darwin {
    program: CString,
    argv: Vec<CString>,
    /// Taken by the spawn and closed when it returns, whether or not it made a child.
    ends: Option<ChildEnds>,
    group: Option<OwnedGroup>,
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
            group: None,
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
        self.group = Some(OwnedGroup::from_spawn(pid)?);
        Ok(())
    }

    fn poll(&mut self) -> Observation {
        let Some(group) = &self.group else {
            return Observation::Lost;
        };
        let pid = group.leader();
        loop {
            let mut status = 0;
            // SAFETY: `status` is valid for one write; WNOHANG never blocks.
            let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if let Some(observation) = classify(waited, status, errno) {
                if matches!(observation, Observation::Reaped(_) | Observation::Lost) {
                    self.group = None;
                }
                return observation;
            }
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        let group = self.group.as_ref().ok_or(io::ErrorKind::NotFound)?;
        match group.retire() {
            Ok(()) => Ok(()),
            Err(RetirementError::Refused(error)) => Err(error),
            Err(RetirementError::Lost) => {
                self.group = None;
                Err(io::ErrorKind::NotFound.into())
            }
        }
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
    /// Starts a new session with only descriptors named by a file action.
    fn close_by_default() -> io::Result<Self> {
        let mut attributes = ptr::null_mut();
        // SAFETY: `attributes` is valid for one write of the opaque handle.
        check(unsafe { libc::posix_spawnattr_init(&raw mut attributes) })?;
        let mut attributes = Self(attributes);
        let flags = libc::c_short::try_from(libc::POSIX_SPAWN_CLOEXEC_DEFAULT)
            .expect("the spawn flag fits a c_short")
            | SET_SESSION;
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

const SOCKET_INT_LEN: libc::socklen_t = 4;

/// Sets one integer `SOL_SOCKET` option on a socket.
pub(super) fn set_socket_int(fd: RawFd, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: `value` is a valid c_int of the stated length for the duration of the call.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&raw const value).cast(),
            SOCKET_INT_LEN,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets `SO_NOSIGPIPE` on a socket.
pub(super) fn no_sigpipe(fd: RawFd) -> io::Result<()> {
    set_socket_int(fd, libc::SO_NOSIGPIPE, 1)
}

/// Waits until `stream` is ready for `events` or reports an error or hang-up, for `left` at most
/// when bounded. An interrupted wait returns early.
pub(super) fn poll(
    stream: &std::os::unix::net::UnixStream,
    events: libc::c_short,
    left: Option<Duration>,
) -> io::Result<()> {
    let mut wait = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    let ms = left.map_or(-1, |left| {
        libc::c_int::try_from(left.as_millis() + 1).unwrap_or(libc::c_int::MAX)
    });
    // SAFETY: `wait` is one valid pollfd for the duration of the call.
    if unsafe { libc::poll(&raw mut wait, 1, ms) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

/// One integer `SOL_SOCKET` option, or None when it can't be read.
#[cfg(test)]
pub(super) fn socket_int(fd: RawFd, name: libc::c_int) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = SOCKET_INT_LEN;
    // SAFETY: `value` and `len` are valid for writes of the stated length.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    (read == 0).then_some(value)
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
#[path = "../unix/brain_group_tests.rs"]
mod group_tests;

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::fd::{FromRawFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::platform::child::Settled;

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
        let pid = child.group.as_ref().expect("group").leader();
        let stopped = stopped_within(pid, Instant::now() + BOUND);
        let before_kill = child.poll();
        let killed = child.kill();
        if killed.is_err() && stopped && child.poll() == Observation::Running {
            super::group_tests::clean_stopped_fixture(pid);
        }
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
        killed.expect("SIGKILL");
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
        for retire_first in [false, true] {
            let mut child = sh("exit 0");
            child.spawn().expect("spawn");
            let pid = child.group.as_ref().expect("group").leader();
            let mut status = 0;
            // SAFETY: `status` is valid for one write; this test plays the foreign reaper.
            let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            assert_eq!(reaped, pid, "the foreign wait reaped it");
            if retire_first {
                assert_eq!(
                    child
                        .kill()
                        .expect_err("pre-signal wait found no child")
                        .kind(),
                    io::ErrorKind::NotFound
                );
                assert!(child.group.is_none(), "no second wait on a lost PID");
            }
            assert_eq!(child.poll(), Observation::Lost);
            assert!(child.group.is_none(), "the lost PID is never reused");
            assert_eq!(
                child.kill().expect_err("no signal after loss").kind(),
                io::ErrorKind::NotFound
            );
        }
    }

    /// An inheritable descriptor above every slot the spawn uses for `ends`, so only
    /// `POSIX_SPAWN_CLOEXEC_DEFAULT` can keep it from the child.
    fn foreign_above(ends: &ChildEnds) -> OwnedFd {
        let above = ends.control.as_raw_fd().max(ends.data.as_raw_fd()).max(4) + 3;
        // SAFETY: duplicating standard error is a plain call; F_DUPFD leaves the copy inheritable.
        let raw = unsafe { libc::fcntl(2, libc::F_DUPFD, above) };
        assert!(raw >= above, "a foreign descriptor");
        // SAFETY: fcntl just returned this descriptor, and nothing else owns it.
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn reap(child: &mut Darwin) -> Observation {
        let by = Instant::now() + BOUND;
        loop {
            match child.poll() {
                Observation::Running if Instant::now() < by => {}
                observed => return observed,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_child_gets_its_legs_on_3_and_4_and_nothing_else() {
        let (shell, child) = pair::pairs().expect("pairs");
        let staged = child.control.as_raw_fd().max(child.data.as_raw_fd()).max(4) + 1;
        let foreign = foreign_above(&child);
        let script = format!(
            "printf c >&3; printf d >&4; for fd in {staged} {} {}; do [ -e /dev/fd/$fd ] && exit 7; done; exit 0",
            staged + 1,
            foreign.as_raw_fd()
        );
        let mut os = Darwin::new(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(&script)],
            child,
        )
        .expect("launch");
        os.spawn().expect("spawn");
        // The adapter is still alive here, so EOF shows it closed the child's ends at the spawn.
        assert_eq!(read_to_eof(&shell.control), b"c");
        assert_eq!(read_to_eof(&shell.data), b"d");
        assert_eq!(reap(&mut os), Observation::Reaped(Exit::Code(0)));
        drop(foreign);
    }

    #[test]
    fn a_failed_spawn_closes_the_childs_ends_at_once() {
        let (shell, child) = pair::pairs().expect("pairs");
        let mut missing =
            Darwin::new(Path::new("/nonexistent/kumochid"), &[], child).expect("launch");
        assert_eq!(missing.spawn().unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(read_to_eof(&shell.control), b"");
        assert_eq!(read_to_eof(&shell.data), b"");
        drop(missing);
    }

    #[test]
    fn start_brain_passes_only_the_parent_bound_argument() {
        let dir = crate::scratch::dir("brain-argv");
        let brain = dir.join("kumochid");
        std::fs::write(
            &brain,
            format!("#!/bin/sh\n[ $# -eq 1 ] && [ \"$1\" = {PARENT_BOUND} ] && exit 0\nexit 1\n"),
        )
        .expect("fixture");
        std::fs::set_permissions(&brain, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("executable");
        let (owner, shell) = start_brain(
            &brain,
            Arc::new(SpawnWord::default()),
            Instant::now() + BOUND,
        )
        .expect("start");
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(0)))
        ));
        assert_eq!(read_to_eof(&shell.control), b"");
    }

    #[test]
    fn start_brain_hands_out_no_ends_when_the_spawn_fails() {
        let refused = start_brain(
            Path::new("/nonexistent/kumochid"),
            Arc::new(SpawnWord::default()),
            Instant::now() + BOUND,
        );
        assert!(matches!(
            refused,
            Err(NotStarted::Refused(Refusal::Spawn(error))) if error.kind() == io::ErrorKind::NotFound
        ));
        let word = Arc::new(SpawnWord::default());
        word.quit();
        assert!(matches!(
            start_brain(Path::new("/bin/test"), word, Instant::now() + BOUND),
            Err(NotStarted::Refused(Refusal::Quitting))
        ));
    }

    /// A spawn that doesn't return until the test lets it, and then makes no child.
    struct Stalled(std::sync::mpsc::Receiver<()>);

    impl Os for Stalled {
        fn auto_reaps(&self) -> bool {
            false
        }
        fn spawn(&mut self) -> io::Result<()> {
            let _released = self.0.recv();
            Err(io::ErrorKind::NotFound.into())
        }
        fn poll(&mut self) -> Observation {
            Observation::Lost
        }
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn only_a_report_proves_whether_a_child_exists() {
        for report in [
            Err(RecvTimeoutError::Disconnected),
            Err(RecvTimeoutError::Timeout),
            Ok(Report::Settled(Settled::Lost)),
        ] {
            let shown = format!("{report:?}");
            assert!(
                matches!(started(report), Err(NotStarted::Pending)),
                "{shown}"
            );
        }
        assert!(matches!(started(Ok(Report::Spawned)), Ok(())));
        assert!(matches!(
            started(Ok(Report::NotSpawned(Refusal::Claimed))),
            Err(NotStarted::Refused(Refusal::Claimed))
        ));
    }

    #[test]
    fn the_spawn_report_waits_no_longer_than_the_attempt() {
        let (release, held) = std::sync::mpsc::channel();
        let by = Instant::now() + Duration::from_millis(100);
        let started = reported(Stalled(held), Arc::new(SpawnWord::default()), by);
        assert!(matches!(started, Err(NotStarted::Pending)));
        assert!(Instant::now() >= by, "waited for the attempt");
        assert!(
            Instant::now() < by + Duration::from_secs(1),
            "and not for the whole spawn report"
        );
        drop(release);
    }
}
