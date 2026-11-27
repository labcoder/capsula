//! macOS custody of leaf workers. A `Job` holds every child it starts by exact pid until a
//! terminal `waitpid`, and its reaper thread, running from `Job::new`, kills and reaps any child
//! still alive once the last `Job` handle drops. Dormant: only test builds can make a `Job`.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::ChildPipes;

/// How often a wait looks at a child again.
const POLL: Duration = Duration::from_millis(10);
/// How long [`Job::terminate`] waits for its children to be reaped.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(5);

/// The brain's custody of the workers it starts. Children stay registered until reaped; the last
/// handle's drop hands those still alive to the reaper thread started with the job.
#[derive(Debug)]
pub struct Job {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// One lock for registration, shutdown and the drop hand-off; never taken under a child's.
    state: Mutex<JobState>,
    /// Wakes the reaper once the last handle drops.
    dropped: Condvar,
}

#[derive(Debug)]
struct JobState {
    refusing: bool,
    dropped: bool,
    /// Strong until reaped, so no handle's drop gives up a reap.
    children: Vec<Arc<Child>>,
}

/// A worker the brain started; its custody stays with the job.
#[derive(Debug)]
pub struct Helper {
    pid: u32,
    child: Arc<Child>,
}

#[derive(Debug)]
struct Child {
    pid: libc::pid_t,
    /// Serializes every wait and signal for this pid.
    status: Mutex<Custody>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Custody {
    Running,
    /// The raw `waitpid` status of an exit or a fatal signal.
    Reaped(libc::c_int),
    /// The pid can't be waited for; latched, and never signalled again.
    Lost,
}

impl Job {
    /// Creates a job and starts its reaper.
    ///
    /// # Errors
    /// `Unsupported` outside test builds; otherwise fails if the reaper thread can't start.
    pub fn new() -> io::Result<Self> {
        if cfg!(not(any(test, feature = "test-support"))) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        Self::start(|reaper| {
            std::thread::Builder::new()
                .name("worker-reaper".into())
                .spawn(reaper)
                .map(drop)
        })
    }

    /// Starts the reaper before the job exists, so no child can outlive a missing one.
    fn start(spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(JobState {
                refusing: false,
                dropped: false,
                children: Vec::new(),
            }),
            dropped: Condvar::new(),
        });
        let reaper = Arc::clone(&shared);
        spawn(Box::new(move || reaper.reap_after_drop()))?;
        Ok(Self { shared })
    }

    /// Whether [`Job::shut_down`] has begun.
    pub(crate) fn is_shut_down(&self) -> bool {
        lock(&self.shared.state).refusing
    }

    /// Refuses new workers from now on, then [`Job::terminate`]s. A start either registered its
    /// child before the refusal, under [`crate::platform::spawn_lock`], and so is ended, or sees
    /// the refusal.
    ///
    /// # Errors
    /// As [`Job::terminate`].
    pub(crate) fn shut_down(&self) -> io::Result<()> {
        {
            let _spawning = crate::platform::spawn_lock();
            lock(&self.shared.state).refusing = true;
        }
        self.terminate()
    }

    /// Kills every registered child that's still running, then waits until each is reaped.
    ///
    /// # Errors
    /// Fails when a child can't be signalled or its custody is lost, or a killed child isn't
    /// reaped within five seconds.
    pub fn terminate(&self) -> io::Result<()> {
        let children = lock(&self.shared.state).children.clone();
        let mut refused = None;
        let mut killed = Vec::with_capacity(children.len());
        for child in &children {
            match child.kill() {
                Ok(()) => killed.push(child),
                Err(error) => {
                    refused.get_or_insert(error);
                }
            }
        }
        let by = Instant::now() + EMPTY_TIMEOUT;
        loop {
            let mut running = false;
            for child in &killed {
                match child.observe() {
                    Custody::Reaped(_) => {}
                    Custody::Running => running = true,
                    Custody::Lost => return Err(io::ErrorKind::NotFound.into()),
                }
            }
            if !running {
                return refused.map_or(Ok(()), Err);
            }
            if Instant::now() >= by {
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(POLL);
        }
    }

    /// Starts a worker with piped stdin and stdout, `/dev/null` on stderr, an empty environment
    /// and `/` as its folder, in the brain's process group. Only descriptors 0 to 2 reach it.
    ///
    /// # Errors
    /// `Unsupported` outside test builds, before any pipe exists; `InvalidInput` for anything but
    /// one job, a relative path or a NUL. Fails when the job was shut down, SIGCHLD wouldn't keep
    /// the child's status, or the pipes or the process can't be made.
    pub fn spawn_piped(exe: &Path, arg: &str, jobs: &[&Self]) -> io::Result<(Helper, ChildPipes)> {
        if cfg!(not(any(test, feature = "test-support"))) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let [job] = jobs else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        if !exe.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let program = CString::new(exe.as_os_str().as_bytes())?;
        let argument = CString::new(arg)?;
        let _spawning = crate::platform::spawn_lock();
        let mut state = lock(&job.shared.state);
        if state.refusing {
            return Err(io::Error::other("the job was shut down"));
        }
        if auto_reaps() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let (stdin, child_stdin) = {
            let (read, write) = io::pipe()?;
            (OwnedFd::from(write), OwnedFd::from(read))
        };
        let (stdout, child_stdout) = {
            let (read, write) = io::pipe()?;
            (OwnedFd::from(read), OwnedFd::from(write))
        };
        let pid = spawn(&program, &argument, &child_stdin, &child_stdout)?;
        let child = Arc::new(Child {
            pid,
            status: Mutex::new(Custody::Running),
        });
        state
            .children
            .retain(|child| !matches!(*lock(&child.status), Custody::Reaped(_)));
        state.children.push(Arc::clone(&child));
        let helper = Helper {
            pid: u32::try_from(pid).map_err(|_| io::ErrorKind::InvalidData)?,
            child,
        };
        let pipes = ChildPipes {
            stdin: File::from(stdin),
            stdout: File::from(stdout),
        };
        Ok((helper, pipes))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        lock(&self.shared.state).dropped = true;
        self.shared.dropped.notify_one();
    }
}

impl Shared {
    /// The reaper's whole life: wait for the last handle's drop, then kill every child still
    /// running once and wait on each exact pid until it's reaped or lost.
    fn reap_after_drop(&self) {
        let mut state = lock(&self.state);
        while !state.dropped {
            state = self
                .dropped
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let children = std::mem::take(&mut state.children);
        drop(state);
        for child in &children {
            if child.kill().is_err() {
                tracing::warn!(target: env!("CARGO_CRATE_NAME"), "a worker couldn't be ended after its job closed");
            }
        }
        while children
            .iter()
            .any(|child| child.observe() == Custody::Running)
        {
            std::thread::sleep(POLL);
        }
    }
}

impl Helper {
    /// The worker's process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Kills the worker unless it's already reaped. Only [`Helper::exits_within`] shows that it
    /// ended.
    ///
    /// # Errors
    /// Fails when its custody is lost, SIGCHLD wouldn't keep its status, or the signal fails.
    pub fn terminate(&self) -> io::Result<()> {
        self.child.kill()
    }

    /// The exit code of a reaped worker; a fatal signal reads as 128 plus its number.
    ///
    /// # Errors
    /// `WouldBlock` while it runs, `NotFound` when its custody is lost.
    pub fn exit_code(&self) -> io::Result<u32> {
        let status = match self.child.observe() {
            Custody::Reaped(status) => status,
            Custody::Running => return Err(io::ErrorKind::WouldBlock.into()),
            Custody::Lost => return Err(io::ErrorKind::NotFound.into()),
        };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            128 + libc::WTERMSIG(status)
        };
        u32::try_from(code).map_err(|_| io::ErrorKind::InvalidData.into())
    }

    /// Whether the worker is reaped within `timeout`.
    ///
    /// # Errors
    /// `NotFound` when its custody is lost.
    pub fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        let by = Instant::now().checked_add(timeout);
        loop {
            match self.child.observe() {
                Custody::Reaped(_) => return Ok(true),
                Custody::Lost => return Err(io::ErrorKind::NotFound.into()),
                Custody::Running => {}
            }
            let left = by.map_or(POLL, |by| by.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return Ok(false);
            }
            std::thread::sleep(left.min(POLL));
        }
    }
}

impl Child {
    /// Reaps the child if it has ended, and reports its custody.
    fn observe(&self) -> Custody {
        let mut status = lock(&self.status);
        if *status == Custody::Running {
            *status = wait(self.pid);
        }
        *status
    }

    /// SIGKILLs the child while it runs, after a fresh wait and a check that the kernel won't
    /// reap it behind the job's back. A reaped child is never signalled.
    fn kill(&self) -> io::Result<()> {
        let mut status = lock(&self.status);
        if *status == Custody::Running {
            *status = wait(self.pid);
        }
        match *status {
            Custody::Reaped(_) => Ok(()),
            Custody::Lost => Err(io::ErrorKind::NotFound.into()),
            Custody::Running if auto_reaps() => Err(io::ErrorKind::Unsupported.into()),
            Custody::Running => signal(self.pid),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One `waitpid(pid, WNOHANG)`, retried while interrupted.
fn wait(pid: libc::pid_t) -> Custody {
    loop {
        let mut status = 0;
        // SAFETY: `status` is valid for one write; WNOHANG never blocks.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if let Some(custody) = classify(waited, status, errno) {
            return custody;
        }
    }
}

/// Classifies one `waitpid(pid, WNOHANG)` result; None means interrupted, so wait again. Only an
/// exit or a fatal signal is a reap: a stopped or continued child still runs.
fn classify(waited: libc::pid_t, status: libc::c_int, errno: libc::c_int) -> Option<Custody> {
    Some(match waited {
        -1 if errno == libc::EINTR => return None,
        ..0 => Custody::Lost,
        0 => Custody::Running,
        _ if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) => Custody::Reaped(status),
        _ => Custody::Running,
    })
}

/// Whether the kernel would reap children itself: SIGCHLD ignored, `SA_NOCLDWAIT`, or a
/// disposition that can't be read.
fn auto_reaps() -> bool {
    #[cfg(test)]
    if faults::REAPS_ITSELF.get() {
        return true;
    }
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null new action only reads the current one into `action`, valid for one write.
    if unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), action.as_mut_ptr()) } != 0 {
        return true;
    }
    // SAFETY: the zeroed value is a valid `sigaction`, and the call filled it.
    let action = unsafe { action.assume_init() };
    reaps_itself(action.sa_sigaction, action.sa_flags)
}

fn reaps_itself(handler: libc::sighandler_t, flags: libc::c_int) -> bool {
    handler == libc::SIG_IGN || flags & libc::SA_NOCLDWAIT != 0
}

fn signal(pid: libc::pid_t) -> io::Result<()> {
    #[cfg(test)]
    if faults::REFUSE_KILL.get() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // SAFETY: a plain signal to a pid this job still holds unreaped, so it can't be reused.
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `posix_spawn`s `[program, argument]` with the two pipe ends on 0 and 1.
fn spawn(
    program: &CString,
    argument: &CString,
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> io::Result<libc::pid_t> {
    let (stdin, stdout) = (stdin.as_raw_fd(), stdout.as_raw_fd());
    // A pipe end on 0 to 2 would be overwritten or kept close-on-exec by the moves below.
    if stdin <= 2 || stdout <= 2 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut actions = FileActions::new()?;
    let raw = &raw mut actions.0;
    // SAFETY: `raw` points to file actions initialized by `new`.
    check(unsafe { libc::posix_spawn_file_actions_adddup2(raw, stdin, 0) })?;
    // SAFETY: as above.
    check(unsafe { libc::posix_spawn_file_actions_adddup2(raw, stdout, 1) })?;
    // SAFETY: as above; the path is a static NUL-terminated string.
    check(unsafe {
        libc::posix_spawn_file_actions_addopen(raw, 2, c"/dev/null".as_ptr(), libc::O_WRONLY, 0)
    })?;
    // SAFETY: as above.
    check(unsafe { libc::posix_spawn_file_actions_addchdir_np(raw, c"/".as_ptr()) })?;
    let mut attributes = Attributes::new()?;
    let flags = libc::c_short::try_from(libc::POSIX_SPAWN_CLOEXEC_DEFAULT)
        .map_err(|_| io::ErrorKind::InvalidData)?;
    // SAFETY: the attributes were initialized by `new`.
    check(unsafe { libc::posix_spawnattr_setflags(&raw mut attributes.0, flags) })?;
    let argv = [
        program.as_ptr().cast_mut(),
        argument.as_ptr().cast_mut(),
        ptr::null_mut(),
    ];
    let envp = [ptr::null_mut()];
    let mut pid = 0;
    // SAFETY: `program` and `argument` are NUL-terminated and outlive the call; `argv` and
    // `envp` end with a null pointer; the file actions and attributes are initialized and live
    // until they drop after the call.
    check(unsafe {
        libc::posix_spawn(
            &raw mut pid,
            program.as_ptr(),
            &raw const actions.0,
            &raw const attributes.0,
            argv.as_ptr(),
            envp.as_ptr(),
        )
    })?;
    Ok(pid)
}

fn check(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
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
    fn new() -> io::Result<Self> {
        let mut attributes = ptr::null_mut();
        // SAFETY: `attributes` is valid for one write of the opaque handle.
        check(unsafe { libc::posix_spawnattr_init(&raw mut attributes) })?;
        Ok(Self(attributes))
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized by `new` and destroyed once.
        unsafe { libc::posix_spawnattr_destroy(&raw mut self.0) };
    }
}

/// Faults a test sets on its own thread; the reaper thread never sees them.
#[cfg(test)]
mod faults {
    use std::cell::Cell;

    thread_local! {
        pub(super) static REAPS_ITSELF: Cell<bool> = const { Cell::new(false) };
        pub(super) static REFUSE_KILL: Cell<bool> = const { Cell::new(false) };
    }
}

#[cfg(test)]
#[path = "macos_tests.rs"]
mod tests;
