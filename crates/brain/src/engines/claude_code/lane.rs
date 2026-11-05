//! The one place `claude` starts. One process at a time, each in its own job nested in the brain
//! job, under this brain's Claude Code lock, after the launch policy's checks; a job admitted
//! under an older generation does nothing. C5a adds questions to this lane.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{CLAUDE_CODE_DIR, CLAUDE_CODE_LOCK};
use kumochi_protocol::message::{ClaudeVersion, PolicyFailed, Unsupported};
use tokio::sync::{mpsc, oneshot};

use super::auth::{self, StatusFailure};
use super::install::{self, FindError};
use super::policy::{self, EntryPoint, LaunchPolicy, OwnedFolders};
use crate::platform::{self, Job, Launch, ManagedPresence, Presence, SessionLock, Stdio};

/// Jobs the lane holds before it refuses more; the core keeps one discovery out at a time.
pub(crate) const LANE_QUEUE: usize = 4;
/// `--version`'s output cap.
pub(crate) const VERSION_CAP: usize = 4 * 1024;
/// `auth status --json`'s output cap.
pub(crate) const STATUS_CAP: usize = 64 * 1024;
/// stderr is read and dropped, up to this.
pub(crate) const STDERR_CAP: usize = 64 * 1024;
/// How long a call may run before it's ended [guess]. Ending it then takes up to the job's own
/// bound to confirm empty, and up to [`PIPE_GRACE`] for each pipe.
pub(crate) const DEADLINE: Duration = Duration::from_secs(10);
/// How often a running call looks at its deadline and at closing.
const POLL: Duration = Duration::from_millis(20);
/// How long the pipes get to end once the call's job is empty.
const PIPE_GRACE: Duration = Duration::from_secs(1);

/// What the lane starts. `prefix` comes before the entry point's own arguments; only the tests'
/// fake has one.
#[derive(Clone)]
pub(crate) struct Program {
    pub(crate) exe: PathBuf,
    pub(crate) prefix: Vec<OsString>,
}

/// Everything the lane reads from outside it.
pub(crate) struct Host {
    /// Finds the install; the brain's looks only in `CLAUDE_CODE_PATHS`.
    pub(crate) find: Box<dyn Fn() -> Result<Program, FindError> + Send>,
    /// A trusted, existing folder: Kumochi's Claude Code folder and the lock go in it.
    pub(crate) base: PathBuf,
    /// Whether this brain holds the vault's session lock.
    pub(crate) vault_held: Arc<AtomicBool>,
    /// Claude Code's managed-policy sources.
    pub(crate) managed: Box<dyn Fn() -> ManagedPresence + Send>,
    /// Every call's job nests in this one.
    pub(crate) brain_job: Arc<Job>,
}

/// A call the core asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Work {
    /// Find the install and read its version.
    Version,
    /// Ask the version found last whether it's signed in.
    Status,
}

/// What a call found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Version(ClaudeVersion),
    Status {
        version: ClaudeVersion,
        signed_in: bool,
    },
}

/// Why a call gave nothing. It carries nothing Claude Code printed and no path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Nothing started: another brain may own Claude Code, an earlier call's processes weren't
    /// confirmed ended, or the lane is closing.
    Refused,
    Find(FindError),
    Unsupported(Unsupported),
    Policy(PolicyFailed),
    /// Kumochi's folders couldn't be made or trusted.
    Folders,
    /// It couldn't be started.
    Spawn,
    /// It didn't exit and close its pipes in time.
    Deadline,
    /// It printed more than its cap.
    TooLong,
    /// A pipe failed before its end.
    Pipe,
    /// `--version` failed or printed no version.
    Version,
    Status(StatusFailure),
}

/// A call's result, for the generation it was admitted under.
#[derive(Debug)]
pub(crate) struct Done {
    pub(crate) generation: u64,
    pub(crate) result: Result<Outcome, Failure>,
}

/// The lane's handle on the core loop.
pub(crate) struct Lane {
    jobs: Option<std_mpsc::SyncSender<(u64, Work)>>,
    current: Arc<AtomicU64>,
    closing: Arc<AtomicBool>,
    done: mpsc::UnboundedReceiver<Done>,
    ended: Option<oneshot::Receiver<bool>>,
}

impl Lane {
    /// Starts the lane's thread.
    ///
    /// # Errors
    /// Fails when the thread can't start.
    pub(crate) fn start(host: Host) -> io::Result<Self> {
        let (jobs, queued) = std_mpsc::sync_channel(LANE_QUEUE);
        let (done_tx, done) = mpsc::unbounded_channel();
        let (ended_tx, ended) = oneshot::channel();
        let current = Arc::new(AtomicU64::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        let mut worker = Worker {
            host,
            guard: None,
            stuck: false,
            found: None,
            current: Arc::clone(&current),
            closing: Arc::clone(&closing),
        };
        std::thread::Builder::new()
            .name("claude-code".to_owned())
            .spawn(move || {
                for (generation, work) in queued {
                    if worker.closing.load(Ordering::Acquire) {
                        break;
                    }
                    // The fence, before anything happens.
                    if generation != worker.current.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = worker.run(work);
                    if done_tx.send(Done { generation, result }).is_err() {
                        break;
                    }
                }
                let emptied = !worker.stuck;
                if !emptied {
                    // A process may still use the folder, so no other brain may take it over.
                    std::mem::forget(worker.guard.take());
                }
                drop(worker);
                ended_tx.send(emptied).ok();
            })?;
        Ok(Self {
            jobs: Some(jobs),
            current,
            closing,
            done,
            ended: Some(ended),
        })
    }

    /// Makes `generation` the only one whose queued jobs still run.
    pub(crate) fn fence(&self, generation: u64) {
        self.current.store(generation, Ordering::Release);
    }

    /// Queues `work` without waiting; false when the lane is full or closed.
    pub(crate) fn submit(&self, generation: u64, work: Work) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.try_send((generation, work)).is_ok())
    }

    /// The next result; never ready once the lane has closed.
    pub(crate) async fn next(&mut self) -> Done {
        match self.done.recv().await {
            Some(done) => done,
            None => std::future::pending().await,
        }
    }

    /// Takes no more work and ends the call running, then waits for the thread by `deadline`.
    /// True only when every call's processes were confirmed ended and the lock was let go.
    pub(crate) async fn close(&mut self, deadline: tokio::time::Instant) -> bool {
        self.closing.store(true, Ordering::Release);
        self.jobs = None;
        let Some(ended) = self.ended.take() else {
            return false;
        };
        matches!(tokio::time::timeout_at(deadline, ended).await, Ok(Ok(true)))
    }
}

impl std::fmt::Debug for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lane")
            .field("generation", &self.current.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

struct Worker {
    host: Host,
    /// Held from the first call until the lane ends, and never let go while a call's processes
    /// may still run.
    guard: Option<SessionLock>,
    /// A call's processes weren't confirmed ended: nothing else starts.
    stuck: bool,
    /// The last install `Version` found, for `Status`.
    found: Option<(Program, ClaudeVersion)>,
    current: Arc<AtomicU64>,
    closing: Arc<AtomicBool>,
}

/// The owned folders and the handles that keep their names from being rebound.
struct Folders {
    paths: OwnedFolders,
    _held: Vec<File>,
}

impl Worker {
    fn run(&mut self, work: Work) -> Result<Outcome, Failure> {
        match work {
            Work::Version => {
                self.found = None;
                let program = (self.host.find)().map_err(Failure::Find)?;
                let folders = self.preflight(None)?;
                let (exit, stdout) =
                    self.call(&program, &folders.paths, &EntryPoint::Version, VERSION_CAP)?;
                let version = (exit == 0)
                    .then(|| install::parse_version(&stdout))
                    .flatten()
                    .ok_or(Failure::Version)?;
                policy::reviewed(version).map_err(Failure::Unsupported)?;
                self.found = Some((program, version));
                Ok(Outcome::Version(version))
            }
            Work::Status => {
                let (program, version) = self
                    .found
                    .clone()
                    .ok_or(Failure::Find(FindError::NotFound))?;
                let reviewed = policy::reviewed(version).map_err(Failure::Unsupported)?;
                let folders = self.preflight(Some(reviewed.policy))?;
                let (exit, stdout) =
                    self.call(&program, &folders.paths, &EntryPoint::Status, STATUS_CAP)?;
                let signed_in = auth::parse_status(&stdout, exit, &folders.paths.config)
                    .map_err(Failure::Status)?;
                Ok(Outcome::Status { version, signed_in })
            }
        }
    }

    /// Everything that must hold before a start: the lane open and not stuck, this brain's
    /// lock, the managed-policy sources (all absent before the version is known, then the
    /// reviewed row's policy), and Kumochi's folders made and held.
    fn preflight(&mut self, policy: Option<LaunchPolicy>) -> Result<Folders, Failure> {
        if self.stuck || self.closing.load(Ordering::Acquire) {
            return Err(Failure::Refused);
        }
        if self.guard.is_none() {
            if !self.host.vault_held.load(Ordering::Acquire) {
                return Err(Failure::Refused);
            }
            let lock = SessionLock::try_acquire(&self.host.base.join(CLAUDE_CODE_LOCK))
                .map_err(|_| Failure::Refused)?;
            self.guard = Some(lock);
        }
        let managed = (self.host.managed)();
        match policy {
            Some(policy) => policy::may_launch(policy, managed).map_err(Failure::Policy)?,
            None if managed
                .all()
                .iter()
                .any(|source| *source != Presence::Absent) =>
            {
                return Err(Failure::Policy(PolicyFailed::ManagedPolicy));
            }
            None => {}
        }
        owned_folders(&self.host.base).map_err(|_| Failure::Folders)
    }

    /// Runs one call to its end: its exit within [`DEADLINE`], then its job emptied and both pipes
    /// ended, each within its own bound. The exit code and stdout, or why not.
    fn call(
        &mut self,
        program: &Program,
        folders: &OwnedFolders,
        entry: &EntryPoint<'_>,
        cap: usize,
    ) -> Result<(u32, Vec<u8>), Failure> {
        let system = platform::system_env().map_err(|_| Failure::Spawn)?;
        let data = policy::launch_data(folders, &system, entry);
        let args: Vec<OsString> = program.prefix.iter().cloned().chain(data.args).collect();
        let job = Job::new().map_err(|_| Failure::Spawn)?;
        let mut spawned = platform::spawn(&Launch {
            exe: &program.exe,
            args: &args,
            cwd: &folders.work,
            env: &data.env,
            stdio: Stdio::Piped,
            jobs: &[&self.host.brain_job, &job],
        })
        .map_err(|_| Failure::Spawn)?;
        drop(spawned.stdin.take());
        let stdout = spawned.stdout.take().map(|pipe| drain(pipe, cap));
        let stderr = spawned.stderr.take().map(|pipe| drain(pipe, STDERR_CAP));
        let deadline = Instant::now() + DEADLINE;
        let exited = loop {
            if self.closing.load(Ordering::Acquire) || Instant::now() >= deadline {
                break false;
            }
            match spawned.process.exits_within(POLL) {
                Ok(true) => break true,
                Ok(false) => {}
                Err(_) => break false,
            }
        };
        let exit = exited.then(|| spawned.process.exit_code().ok()).flatten();
        // Whatever the root left running ends here, and nothing else starts until it has.
        if job.terminate().is_err() {
            self.stuck = true;
            tracing::warn!("a claude call's processes didn't end; Claude Code stays closed");
            return Err(Failure::Refused);
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(PIPE_GRACE);
        let stdout = stdout.and_then(|pipe| pipe.recv_timeout(remaining).ok());
        let stderr = stderr.and_then(|pipe| pipe.recv_timeout(PIPE_GRACE).ok());
        let exit = exit.ok_or(Failure::Deadline)?;
        Ok((exit, output(stdout, stderr)?))
    }
}

/// Kumochi's Claude Code folder and the folders in it, each owner-only, made if missing and
/// held, under `base` held from the volume root.
fn owned_folders(base: &Path) -> Result<Folders, platform::Untrusted> {
    let mut held = platform::hold_chain(base)?;
    let root = base.join(CLAUDE_CODE_DIR);
    held.push(platform::owner_only_dir(&root)?);
    let paths = OwnedFolders {
        config: root.join("config"),
        profile: root.join("profile"),
        appdata: root.join("appdata"),
        local_appdata: root.join("local"),
        tmp: root.join("tmp"),
        work: root.join("work"),
    };
    for dir in [
        &paths.config,
        &paths.profile,
        &paths.appdata,
        &paths.local_appdata,
        &paths.tmp,
        &paths.work,
    ] {
        held.push(platform::owner_only_dir(dir)?);
    }
    Ok(Folders { paths, _held: held })
}

/// A pipe read to its end: the first `cap` bytes, whether there were more, and whether a read
/// failed before the end.
struct Drained {
    bytes: Vec<u8>,
    over: bool,
    broken: bool,
}

/// stdout's bytes, when both pipes reached their end, unbroken and within their caps.
fn output(stdout: Option<Drained>, stderr: Option<Drained>) -> Result<Vec<u8>, Failure> {
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(Failure::Deadline);
    };
    if stdout.broken || stderr.broken {
        return Err(Failure::Pipe);
    }
    if stdout.over || stderr.over {
        return Err(Failure::TooLong);
    }
    Ok(stdout.bytes)
}

/// Reads `pipe` to its end on a thread, keeping at most `cap` bytes; the receiver gets them once
/// it has ended.
fn drain(mut pipe: impl Read + Send + 'static, cap: usize) -> std_mpsc::Receiver<Drained> {
    let (done, drained) = std_mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("claude-code-pipe".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut over = false;
            let mut broken = false;
            let mut chunk = [0; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        broken = true;
                        break;
                    }
                    Ok(read) => {
                        let room = (cap + 1).saturating_sub(bytes.len()).min(read);
                        bytes.extend_from_slice(&chunk[..room]);
                        over |= bytes.len() > cap;
                    }
                }
            }
            bytes.truncate(cap);
            done.send(Drained {
                bytes,
                over,
                broken,
            })
            .ok();
        });
    // With no reader thread the receiver gets nothing, which ends the call as `Deadline`.
    drop(spawned);
    drained
}

#[cfg(test)]
pub(crate) mod tests;
