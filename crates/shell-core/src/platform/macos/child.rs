//! The Darwin system calls behind the shell's brain child: `posix_spawn`, an exact-pid
//! `waitpid(WNOHANG)`, SIGKILL and a read of SIGCHLD's disposition. The custody rules are the
//! shared owner's (`platform::child`); this file only makes the calls and classifies their results.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;

use crate::platform::child::{Exit, Observation, Os};

/// A child to start with `posix_spawn`: an absolute program, its arguments and an empty
/// environment, so nothing of the shell's environment is inherited.
#[derive(Debug)]
pub(crate) struct Darwin {
    program: CString,
    argv: Vec<CString>,
    pid: Option<libc::pid_t>,
}

impl Darwin {
    /// # Errors
    /// `InvalidInput` when the program isn't absolute or any string holds a NUL.
    pub(crate) fn new(program: &Path, arguments: &[&OsStr]) -> io::Result<Self> {
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
        let argv = null_terminated(&self.argv);
        let envp = [ptr::null_mut()];
        let mut pid = 0;
        // SAFETY: `program` and every `argv` entry are NUL-terminated strings that outlive the
        // call; `argv` and `envp` end with a null pointer; null file actions and attributes take
        // the defaults.
        let error = unsafe {
            libc::posix_spawn(
                &raw mut pid,
                self.program.as_ptr(),
                ptr::null(),
                ptr::null(),
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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::platform::child::{ChildOwner, Refusal, Report, Settled, SpawnWord};

    const BOUND: Duration = Duration::from_secs(10);

    fn sh(script: &str) -> Darwin {
        Darwin::new(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(script)],
        )
        .expect("launch")
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
        let relative = Darwin::new(Path::new("bin/sh"), &[]).unwrap_err();
        assert_eq!(relative.kind(), io::ErrorKind::InvalidInput);
        let nul = Darwin::new(Path::new("/bin/sh"), &[OsStr::new("a\0b")]).unwrap_err();
        assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure_and_frees_the_claim() {
        let word = Arc::new(SpawnWord::default());
        let missing = Darwin::new(Path::new("/nonexistent/kumochid"), &[]).expect("launch");
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
}
