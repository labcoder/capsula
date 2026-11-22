//! Claude Code's question launches, run by the lane's worker from their turn to their teardown.
//! A launch is one `claude -p` in its own run job, with its own folder and bridge pipe. A
//! [`RunLease`] holds it, and revoking or dropping the lease ends it. Teardown happens on the
//! lane's thread, never inside an engine future, and it ends the job before it deletes anything.
//! A folder a stopped brain left behind is swept under the lane's lock before anything starts.
//! macOS counterpart: unverified, behind `platform`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, PipeReader, PipeWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{CLAUDE_CODE_DIR, ENGINE_DIR, MCP_SERVER_NAME};
use kumochi_protocol::message::ThreadId;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::audit::AuditGate;
use super::lane::{Failure, Folders, Host, PIPE_GRACE, POLL, Program};
use super::policy::{self, EntryPoint, OwnedFolders};
use super::seed::pictured::PicturedSeed;
use crate::bridge::LaunchId;
use crate::bridge::listener::{Binding, LaunchContext, Listener, McpCall};
use crate::connections::model::Lease as _;
use crate::context::ToolSchema;
use crate::coordinator::PictureWrite;
use crate::engines::{PictureReceipt, SendLease};
use crate::platform::{self, Job, Stdio};

/// The longest a launch may live, from admission [guess, not measured: C5b's 30 s warm window
/// plus the 120 s question].
pub(crate) const MAX_LAUNCH: Duration = Duration::from_secs(150);
/// stdin and stdout chunks in flight each way.
pub(crate) const PIPE_QUEUE: usize = 4;
/// The most one stdout chunk holds, and the most one stdin chunk does.
pub(crate) const CHUNK: usize = 64 * 1024;
/// The seed's cap, in encoded bytes: the whole JSON line and its newline (the providers' request
/// cap).
pub(crate) const MAX_SEED: usize = 16 * 1024 * 1024;
/// Both control lines together.
pub(crate) const MAX_CONTROL: usize = 4 * 1024;
/// How long a launch's teardown gets, past its deadline, to confirm its job empty before its held
/// send fails closed [guess: C5a's configured waits add up to about 12 s].
pub(crate) const SETTLE: Duration = Duration::from_secs(15);

/// stdin may admit a write.
const OPEN: u8 = 0;
/// A write was admitted and hasn't returned.
const WRITING: u8 = 1;
/// Revoked: nothing more is admitted.
const REVOKED: u8 = 2;
/// Revoked while an admitted write hadn't returned.
const REVOKED_WRITING: u8 = 3;

/// What C5b's snapshot fixes for one launch. Nothing in it is user content.
pub(crate) struct Question {
    /// The thread whose deletion revokes it.
    pub(crate) thread: ThreadId,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    /// The tools frozen with the launch.
    pub(crate) tools: Arc<[ToolSchema]>,
    /// How long it may live from admission; at most [`MAX_LAUNCH`].
    pub(crate) lifetime: Duration,
    /// Where its listener hands admitted calls.
    pub(crate) calls: Option<mpsc::Sender<McpCall>>,
}

/// What the round sends on stdin.
pub(crate) enum Input {
    Chunk(Vec<u8>),
    Picture(PictureChunk),
    /// Closes stdin, so the child reads its end.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "C5b's engine ends the first question")
    )]
    End,
    /// The revocation's: the pump stops waiting for chunks.
    Wake,
}

pub(crate) struct PictureChunk {
    bytes: Zeroizing<Vec<u8>>,
    until: Instant,
    first: Option<oneshot::Sender<Result<(), StdinRefused>>>,
}

/// Kinds and lengths only: a chunk is the user's question.
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chunk(bytes) => f
                .debug_struct("Chunk")
                .field("bytes", &bytes.len())
                .finish(),
            Self::Picture(chunk) => f
                .debug_struct("Picture")
                .field("bytes", &chunk.bytes.len())
                .finish(),
            Self::End => f.write_str("End"),
            Self::Wake => f.write_str("Wake"),
        }
    }
}

/// The round's ends of a live launch's pipes. stdin goes only through the one [`StdinWriter`].
pub(crate) struct Pipes {
    stdin: Option<mpsc::Sender<Input>>,
    control: Arc<Control>,
    pub(crate) stdout: mpsc::Receiver<Zeroizing<Vec<u8>>>,
}

impl Pipes {
    /// The launch's one stdin writer; `None` once it's been taken.
    pub(crate) fn writer(&mut self) -> Option<StdinWriter> {
        Some(StdinWriter {
            sender: self.stdin.take()?,
            control: Arc::clone(&self.control),
        })
    }

    /// The raw channel, for the supervisor's own tests of the pumps.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &mpsc::Sender<Input> {
        self.stdin.as_ref().expect("the writer wasn't taken")
    }
}

/// Why stdin took nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StdinRefused {
    /// Over [`MAX_SEED`] or [`MAX_CONTROL`]; nothing of it was queued.
    TooLarge,
    /// The launch's one seed was asked for already.
    Second,
    /// The launch was revoked, or its stdin pump is gone.
    Revoked,
    /// The deadline passed while it waited for room.
    Deadline,
}

/// Writes a launch's stdin: the control lines, then one seed, each checked whole against its cap
/// before anything is queued, and queued in chunks of at most [`CHUNK`]. The queue holds
/// [`PIPE_QUEUE`] chunks, so at most 256 KiB of payload waits there; the seed itself, the pump's
/// buffer and the OS pipe come on top.
pub(crate) struct StdinWriter {
    sender: mpsc::Sender<Input>,
    control: Arc<Control>,
}

impl StdinWriter {
    /// One control line.
    ///
    /// # Errors
    /// [`StdinRefused::TooLarge`] once the control lines pass [`MAX_CONTROL`]; otherwise as
    /// [`seed`](Self::seed) does while it queues.
    pub(crate) async fn control(
        &mut self,
        line: &[u8],
        until: Instant,
    ) -> Result<(), StdinRefused> {
        let total = self.control.line_bytes.load(Ordering::Acquire) + line.len();
        if total > MAX_CONTROL {
            return Err(StdinRefused::TooLarge);
        }
        // One chunk, since MAX_CONTROL < CHUNK: charged only once it's queued, so a wait that
        // ends or is dropped first leaves the allowance as it was.
        self.enqueue(until, || Input::Chunk(line.to_vec())).await?;
        self.control.line_bytes.store(total, Ordering::Release);
        Ok(())
    }

    /// The launch's one seed. Asking uses it up, refused or not.
    ///
    /// # Errors
    /// [`StdinRefused::Second`] after the first ask, [`StdinRefused::TooLarge`] over
    /// [`MAX_SEED`], [`StdinRefused::Revoked`] once the launch is revoked, and
    /// [`StdinRefused::Deadline`] when `until` passes while it waits for room. Chunks queued
    /// before a revocation or the deadline stay queued; the revoked launch never runs them.
    pub(crate) async fn seed(&mut self, seed: &[u8], until: Instant) -> Result<(), StdinRefused> {
        self.start_seed(seed.len())?;
        for chunk in seed.chunks(CHUNK) {
            self.enqueue(until, || Input::Chunk(chunk.to_vec())).await?;
        }
        Ok(())
    }

    pub(super) async fn picture(
        &mut self,
        seed: &PicturedSeed,
        until: Instant,
    ) -> Result<(), StdinRefused> {
        self.start_seed(seed.bytes().len())?;
        let mut chunks = seed.bytes().chunks(CHUNK);
        let first = chunks.next().ok_or(StdinRefused::Revoked)?;
        let (sent, written) = oneshot::channel();
        self.enqueue(until, || {
            Input::Picture(PictureChunk {
                bytes: Zeroizing::new(first.to_vec()),
                until,
                first: Some(sent),
            })
        })
        .await?;
        tokio::time::timeout_at(until.into(), written)
            .await
            .map_err(|_| StdinRefused::Deadline)?
            .map_err(|_| StdinRefused::Revoked)??;
        for chunk in chunks {
            self.enqueue(until, || {
                Input::Picture(PictureChunk {
                    bytes: Zeroizing::new(chunk.to_vec()),
                    until,
                    first: None,
                })
            })
            .await?;
        }
        Ok(())
    }

    fn start_seed(&self, len: usize) -> Result<(), StdinRefused> {
        if self.control.seeded.swap(true, Ordering::AcqRel) {
            return Err(StdinRefused::Second);
        }
        if len > MAX_SEED {
            return Err(StdinRefused::TooLarge);
        }
        Ok(())
    }

    /// Closes stdin once what's queued is written, if there's room to say so; teardown ends the
    /// child either way.
    pub(crate) fn end(self) {
        self.sender.try_send(Input::End).ok();
    }

    /// Takes room before making the owned chunk.
    async fn enqueue(
        &self,
        until: Instant,
        chunk: impl FnOnce() -> Input,
    ) -> Result<(), StdinRefused> {
        if self.control.is_revoked() {
            return Err(StdinRefused::Revoked);
        }
        let permit = match tokio::time::timeout_at(until.into(), self.sender.reserve()).await {
            Ok(Err(_)) => return Err(StdinRefused::Revoked),
            Ok(Ok(permit)) if Instant::now() < until => permit,
            Err(_) | Ok(Ok(_)) => return Err(StdinRefused::Deadline),
        };
        if self.control.is_revoked() {
            return Err(StdinRefused::Revoked);
        }
        permit.send(chunk());
        Ok(())
    }
}

/// The granted send a launch holds until its job is confirmed empty.
enum SendSlot {
    Open(Option<SendLease>),
    /// Its teardown began: nothing more is taken.
    Sealed,
}

/// One launch's shared state: the lease, the core, the worker and the stdin pump hold it.
pub(crate) struct Control {
    /// Write admission and revocation, linearized: every change is one compare-exchange.
    state: AtomicU8,
    /// The round wants no more stdout, so the stdout pump drops what it reads.
    released: AtomicBool,
    /// Its teardown is over, whatever it came to.
    ended: AtomicBool,
    output_closed: AtomicBool,
    gate: AuditGate,
    /// The stdin channel, for the revocation's [`Input::Wake`].
    wake: mpsc::Sender<Input>,
    /// The send this launch's seed went out under, until its job is confirmed empty.
    send: Mutex<SendSlot>,
    /// The one seed was asked for.
    seeded: AtomicBool,
    /// The control lines' bytes so far.
    line_bytes: AtomicUsize,
    #[cfg(test)]
    pub(crate) picture_hooks: Mutex<tests::Hooks>,
}

impl Control {
    /// Takes `lease` until the job is confirmed empty, or gives it back when the launch is
    /// revoked, its teardown began, or it holds one already.
    fn hold_send(&self, lease: SendLease) -> Result<(), SendLease> {
        let mut slot = self.send.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *slot {
            SendSlot::Open(held @ None) if !self.is_revoked() => {
                *held = Some(lease);
                Ok(())
            }
            _ => Err(lease),
        }
    }

    /// Seals the slot and takes what it held.
    fn seal_send(&self) -> Option<SendLease> {
        let mut slot = self.send.lock().unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *slot, SendSlot::Sealed) {
            SendSlot::Open(held) => held,
            SendSlot::Sealed => None,
        }
    }

    /// Fails the gate and admits no more stdin. A chunk admitted before is in flight, and the
    /// teardown waits for it. Never blocks.
    fn revoke(&self) -> Revoked {
        self.gate.fail();
        let mut state = self.state.load(Ordering::Acquire);
        let revoked = loop {
            let (next, revoked) = match state {
                OPEN => (REVOKED, Revoked::Idle),
                WRITING => (REVOKED_WRITING, Revoked::WriteInFlight),
                _ => return Revoked::Already,
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break revoked,
                Err(now) => state = now,
            }
        };
        // A full channel means the pump has chunks to take, and each one's admission fails now.
        self.wake.try_send(Input::Wake).ok();
        revoked
    }

    /// The round is done with the launch: revoked, and its stdout dropped from now on.
    pub(crate) fn withdraw(&self) -> Revoked {
        let revoked = self.revoke();
        self.released.store(true, Ordering::Release);
        revoked
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.state.load(Ordering::Acquire) >= REVOKED
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Whether the listener would admit a call by the gate.
    pub(crate) fn may_call(&self) -> bool {
        self.gate.may_call()
    }

    /// The launch's gate, which its revocation and its teardown fail.
    pub(crate) fn gate(&self) -> &AuditGate {
        &self.gate
    }

    /// The instant a chunk is admitted, or false once revoked.
    fn admit(&self) -> bool {
        self.state
            .compare_exchange(OPEN, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn admit_picture(&self, until: Instant) -> Result<PictureWrite, StdinRefused> {
        let mut slot = self.send.lock().unwrap_or_else(PoisonError::into_inner);
        let SendSlot::Open(Some(lease)) = &mut *slot else {
            return Err(StdinRefused::Revoked);
        };
        let grant = lease.picture.as_ref().ok_or(StdinRefused::Revoked)?;
        let permit = grant.reserve_write().ok_or(StdinRefused::Revoked)?;
        if self.is_revoked() || self.output_closed.load(Ordering::Acquire) {
            return Err(StdinRefused::Revoked);
        }
        if Instant::now() >= until {
            return Err(StdinRefused::Deadline);
        }
        lease.wrote();
        if let Some(receipt) = lease.picture_receipt.take() {
            receipt.send(PictureReceipt::Attempted).ok();
        }
        Ok(permit)
    }

    /// The admitted write returned; false when it was revoked meanwhile.
    fn written(&self) -> bool {
        if self
            .state
            .compare_exchange(WRITING, OPEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
        self.state.store(REVOKED, Ordering::Release);
        false
    }
}

/// The state and flags only.
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control")
            .field("state", &self.state.load(Ordering::Relaxed))
            .field("ended", &self.is_ended())
            .finish_non_exhaustive()
    }
}

/// The run's hold on a launch; dropping it revokes the launch.
pub(crate) struct RunLease(Arc<Control>);

impl RunLease {
    /// Revokes the launch at once, linearized with stdin's write admission: after `Idle` no
    /// write is admitted and none is in flight; after `WriteInFlight` one admitted write may
    /// still reach the child, and the teardown waits for it. Never blocks.
    pub(crate) fn revoke(&self) -> Revoked {
        self.0.withdraw()
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// What a revocation found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revoked {
    /// No write in flight, and none admitted from now on.
    Idle,
    /// A write admitted before it hadn't returned.
    WriteInFlight,
    /// It was revoked already.
    Already,
}

/// Why a launch wasn't queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchRefused {
    /// Claude Code isn't ready, or sign-in isn't decided, or it's off, or a Disconnect is
    /// pending.
    NotReady,
    /// Its lifetime is over [`MAX_LAUNCH`].
    Lifetime,
    /// The lane's queue is full.
    Busy,
    /// No launch id could be drawn.
    Identity,
}

/// Why a queued launch never ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchFailed {
    /// Revoked before its turn.
    Revoked,
    /// Its deadline passed before its turn.
    Expired,
    /// The lane refused it, as it would a Status call.
    Lane(Failure),
    /// It couldn't be started.
    Spawn,
    /// Its bridge pipe couldn't be created.
    Listener,
}

/// How a launch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Teardown {
    /// Its job was confirmed empty, its pumps ended and its folder is gone.
    Done,
    /// As `Done`, but the folder is left for the next sweep.
    FolderLeft,
    /// Its processes or pumps weren't confirmed ended: the lane is stuck and the folder kept.
    Stuck,
}

/// A launch admitted by the core, queued or running.
pub(crate) struct Launch {
    id: LaunchId,
    deadline: Instant,
    binding: Binding,
    lease: RunLease,
    started: Option<oneshot::Receiver<Result<Pipes, LaunchFailed>>>,
    ended: oneshot::Receiver<Teardown>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "C5b's engine binds and seeds the first launch")
)]
impl Launch {
    pub(crate) fn lease(&self) -> &RunLease {
        &self.lease
    }

    /// Drawn at admission.
    pub(crate) fn id(&self) -> LaunchId {
        self.id
    }

    /// The absolute deadline fixed at admission.
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The run the listener admits calls for, set once.
    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }

    /// Whether the listener admits a call.
    pub(crate) fn gate(&self) -> &AuditGate {
        &self.lease.0.gate
    }

    /// Moves the seed's granted send into the launch: it settles only once the launch's job is
    /// confirmed empty and its pumps ended, and never when they weren't.
    ///
    /// # Errors
    /// Gives `lease` back, unused, when the launch is revoked, its teardown began or it holds a
    /// send already; nothing may be queued under it then.
    pub(crate) fn hold_send(&self, lease: SendLease) -> Result<(), SendLease> {
        self.lease.0.hold_send(lease)
    }

    /// The pipes once it runs, or why it never did; `None` after the first answer. The future
    /// borrows nothing, so the audit task can own it.
    pub(crate) fn started(
        &mut self,
    ) -> impl Future<Output = Option<Result<Pipes, LaunchFailed>>> + Send + 'static {
        let started = self.started.take();
        async move {
            // A lane gone before the launch's turn refused it.
            Some(
                started?
                    .await
                    .unwrap_or(Err(LaunchFailed::Lane(Failure::Refused))),
            )
        }
    }

    /// Revokes the launch and waits for its teardown.
    pub(crate) async fn end(self) -> Teardown {
        let Self { lease, ended, .. } = self;
        drop(lease);
        // A worker that dropped it without an answer can't have confirmed anything.
        ended.await.unwrap_or(Teardown::Stuck)
    }
}

/// A launch as the lane's queue holds it.
pub(crate) struct Start {
    id: LaunchId,
    question: Question,
    /// Fixed at admission; waiting in the queue never extends it.
    deadline: Instant,
    control: Arc<Control>,
    binding: Binding,
    stdin: mpsc::Receiver<Input>,
    round_stdin: mpsc::Sender<Input>,
    started: oneshot::Sender<Result<Pipes, LaunchFailed>>,
    ended: oneshot::Sender<Teardown>,
    #[cfg(test)]
    pub(crate) hooks: tests::Hooks,
}

impl Start {
    pub(crate) fn control(&self) -> Arc<Control> {
        Arc::clone(&self.control)
    }

    /// Whether it may start now.
    pub(super) fn due(&self, closing: &AtomicBool) -> Result<(), LaunchFailed> {
        if self.control.is_revoked() {
            Err(LaunchFailed::Revoked)
        } else if closing.load(Ordering::Acquire) {
            Err(LaunchFailed::Lane(Failure::Refused))
        } else if Instant::now() >= self.deadline {
            Err(LaunchFailed::Expired)
        } else {
            Ok(())
        }
    }

    /// Answers a launch that never starts: nothing was made, so its teardown is done.
    pub(super) fn refuse(self, why: LaunchFailed) {
        self.control.revoke();
        // Nothing ran, so nothing was sent under it.
        drop(self.control.seal_send());
        self.started.send(Err(why)).ok();
        self.control.ended.store(true, Ordering::Release);
        self.ended.send(Teardown::Done).ok();
    }
}

/// A launch for `question`, its id drawn and its deadline fixed now, and the lane's half of it.
///
/// # Errors
/// [`LaunchRefused::Identity`] when no id can be drawn.
pub(crate) fn admit(question: Question) -> Result<(Start, Launch), LaunchRefused> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|_| LaunchRefused::Identity)?;
    let id = LaunchId(u128::from_le_bytes(id));
    let deadline = Instant::now() + question.lifetime;
    let (round_stdin, stdin) = mpsc::channel(PIPE_QUEUE);
    let control = Arc::new(Control {
        state: AtomicU8::new(OPEN),
        released: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        output_closed: AtomicBool::new(false),
        gate: AuditGate::new(),
        wake: round_stdin.clone(),
        send: Mutex::new(SendSlot::Open(None)),
        seeded: AtomicBool::new(false),
        line_bytes: AtomicUsize::new(0),
        #[cfg(test)]
        picture_hooks: Mutex::default(),
    });
    let (started_tx, started) = oneshot::channel();
    let (ended_tx, ended) = oneshot::channel();
    let binding = Binding::default();
    let start = Start {
        id,
        question,
        deadline,
        control: Arc::clone(&control),
        binding: binding.clone(),
        stdin,
        round_stdin,
        started: started_tx,
        ended: ended_tx,
        #[cfg(test)]
        hooks: tests::Hooks::default(),
    };
    let launch = Launch {
        id,
        deadline,
        binding,
        lease: RunLease(control),
        started: Some(started),
        ended,
    };
    Ok((start, launch))
}

/// What the lane checked before a launch's own folder: the install and Kumochi's folders.
pub(super) struct Ready {
    pub(super) program: Program,
    pub(super) folders: Folders,
}

/// Runs `start` from its turn to its teardown, on the lane's thread.
pub(super) fn run(
    start: Start,
    ready: Result<Ready, Failure>,
    host: &Host,
    closing: &AtomicBool,
) -> Teardown {
    let Start {
        id,
        question,
        deadline,
        control,
        binding,
        stdin,
        round_stdin,
        started,
        ended,
        #[cfg(test)]
        mut hooks,
    } = start;
    let mut running = Running {
        control: Arc::clone(&control),
        parents: Vec::new(),
        held: Vec::new(),
        dir: None,
        listener: None,
        job: None,
        child: None,
        pumps: Vec::new(),
        #[cfg(test)]
        terminate: hooks.terminate.take(),
    };
    let begun = ready.map_err(LaunchFailed::Lane).and_then(|ready| {
        let parts = Parts {
            id,
            question,
            deadline,
            control: &control,
            binding,
            stdin,
            round_stdin,
            #[cfg(test)]
            hooks,
        };
        begin(&mut running, ready, parts, host)
    });
    match begun {
        Err(failure) => {
            started.send(Err(failure)).ok();
        }
        Ok(pipes) => {
            // A round that's gone revoked the launch as it went.
            started.send(Ok(pipes)).ok();
            if let Some(child) = &running.child {
                while !control.is_revoked()
                    && !closing.load(Ordering::Acquire)
                    && Instant::now() < deadline
                    && matches!(child.exits_within(POLL), Ok(false))
                {}
            }
        }
    }
    let teardown = running.stop(host);
    control.ended.store(true, Ordering::Release);
    ended.send(teardown).ok();
    teardown
}

/// What `begin` takes from the launch.
struct Parts<'a> {
    id: LaunchId,
    question: Question,
    deadline: Instant,
    control: &'a Arc<Control>,
    binding: Binding,
    stdin: mpsc::Receiver<Input>,
    round_stdin: mpsc::Sender<Input>,
    #[cfg(test)]
    hooks: tests::Hooks,
}

/// Makes the launch's folder, its bridge pipe and job, then starts it and its pumps. Everything
/// made goes into `running` as it's made, so a failure part way stops what exists, in order.
fn begin(
    running: &mut Running,
    ready: Ready,
    parts: Parts<'_>,
    host: &Host,
) -> Result<Pipes, LaunchFailed> {
    let Parts {
        id,
        question,
        deadline,
        control,
        binding,
        stdin,
        round_stdin,
        #[cfg(test)]
        hooks,
    } = parts;
    #[cfg(test)]
    if let Some(sent) = &hooks.id {
        sent.send(id).ok();
    }
    let Ready { program, folders } = ready;
    running.parents = folders.held;
    let (work, tmp) = make_folder(running, host, id)?;
    #[cfg(test)]
    tests::pause(hooks.folder);
    // The pipe registers with the runtime that serves it.
    let listener = {
        let _runtime = host.runtime.enter();
        Listener::create(id).map_err(|_| LaunchFailed::Listener)?
    };
    let mcp_config = work.join("mcp.json");
    let config = mcp_json(&host.brain_exe, id).ok_or(LaunchFailed::Spawn)?;
    std::fs::write(&mcp_config, config).map_err(|_| LaunchFailed::Lane(Failure::Folders))?;
    let job = Arc::new(Job::new().map_err(|_| LaunchFailed::Spawn)?);
    running.job = Some(Arc::clone(&job));
    let allowed: BTreeSet<String> = question
        .tools
        .iter()
        .map(|tool| format!("mcp__{MCP_SERVER_NAME}__{}", tool.name))
        .collect();
    let paths = OwnedFolders {
        work: work.clone(),
        tmp,
        ..folders.paths
    };
    let system = platform::system_env().map_err(|_| LaunchFailed::Spawn)?;
    let data = policy::launch_data(
        &paths,
        &system,
        &EntryPoint::Question {
            mcp_config: &mcp_config,
            allowed_tools: &allowed,
            model: &question.model,
            effort: question.effort.as_deref(),
            max_turns: policy::native_turns(),
        },
    );
    let args: Vec<OsString> = program.prefix.iter().cloned().chain(data.args).collect();
    // Served before the start, so the bridge finds it.
    let context = Context {
        job: Arc::clone(&job),
        brain_exe: host.brain_exe.clone(),
        deadline,
        tools: question.tools,
        gate: control.gate.clone(),
        binding,
        calls: question.calls,
    };
    running.listener = Some(serve(listener, context, &host.runtime));
    let spawned = platform::spawn(&platform::Launch {
        exe: &program.exe,
        args: &args,
        cwd: &work,
        env: &data.env,
        stdio: Stdio::Piped,
        jobs: &[&host.brain_job, &job],
    })
    .map_err(|_| LaunchFailed::Spawn)?;
    running.child = Some(spawned.process);
    let stdout = pumps(
        running,
        (spawned.stdin, spawned.stdout, spawned.stderr),
        stdin,
        control,
        #[cfg(test)]
        tests::Pumps {
            stdin: tests::Stdin {
                admit: hooks.admit,
                write: hooks.write,
                writes: hooks.writes,
                entries: hooks.entries,
            },
            report: hooks.pumps,
            fails: hooks.spawn_fails,
        },
    )?;
    Ok(Pipes {
        stdin: Some(round_stdin),
        control: Arc::clone(control),
        stdout,
    })
}

/// Starts the three pumps on the child's pipes, each into `running` as it starts; the round's
/// stdout.
fn pumps(
    running: &mut Running,
    child_pipes: (Option<PipeWriter>, Option<PipeReader>, Option<PipeReader>),
    stdin: mpsc::Receiver<Input>,
    control: &Arc<Control>,
    #[cfg(test)] hooks: tests::Pumps,
) -> Result<mpsc::Receiver<Zeroizing<Vec<u8>>>, LaunchFailed> {
    let (Some(child_stdin), Some(child_stdout), Some(child_stderr)) = child_pipes else {
        return Err(LaunchFailed::Spawn);
    };
    let (output, stdout) = mpsc::channel(PIPE_QUEUE);
    let writer = Arc::clone(control);
    let reader = Arc::clone(control);
    #[cfg(test)]
    let tests::Pumps {
        stdin: stdin_hooks,
        report,
        fails,
    } = hooks;
    running.pumps.push(
        pump(
            "claude-code-stdin",
            move || {
                write_input(
                    child_stdin,
                    stdin,
                    &writer,
                    #[cfg(test)]
                    stdin_hooks,
                );
            },
            #[cfg(test)]
            (report.clone(), fails),
        )
        .map_err(|_| LaunchFailed::Spawn)?,
    );
    running.pumps.push(
        pump(
            "claude-code-stdout",
            move || read_output(child_stdout, &output, &reader),
            #[cfg(test)]
            (report.clone(), fails),
        )
        .map_err(|_| LaunchFailed::Spawn)?,
    );
    running.pumps.push(
        pump(
            "claude-code-stderr",
            move || drain_stderr(child_stderr),
            #[cfg(test)]
            (report, fails),
        )
        .map_err(|_| LaunchFailed::Spawn)?,
    );
    Ok(stdout)
}

/// The engine folder, held with Kumochi's, and the launch's own owner-only folder in it with
/// its `work` and `tmp` folders, held apart so the teardown can let them go.
fn make_folder(
    running: &mut Running,
    host: &Host,
    id: LaunchId,
) -> Result<(PathBuf, PathBuf), LaunchFailed> {
    let folder = |path: &Path| {
        platform::owner_only_dir(path).map_err(|_| LaunchFailed::Lane(Failure::Folders))
    };
    let engine = host.base.join(CLAUDE_CODE_DIR).join(ENGINE_DIR);
    running.parents.push(folder(&engine)?);
    let dir = engine.join(id.hex());
    let work = dir.join("work");
    let tmp = dir.join("tmp");
    // Named before it's made, so a failure part way still deletes whatever was.
    running.dir = Some(dir.clone());
    for path in [&dir, &work, &tmp] {
        running.held.push(folder(path)?);
    }
    Ok((work, tmp))
}

/// What the listener's task owns.
struct Context {
    job: Arc<Job>,
    brain_exe: PathBuf,
    deadline: Instant,
    tools: Arc<[ToolSchema]>,
    gate: AuditGate,
    binding: Binding,
    calls: Option<mpsc::Sender<McpCall>>,
}

/// Serves `listener` on `runtime` until its session ends or it's aborted; the receiver hears
/// the task end, however it ends.
fn serve(
    listener: Listener,
    context: Context,
    runtime: &tokio::runtime::Handle,
) -> (tokio::task::AbortHandle, std_mpsc::Receiver<()>) {
    let (served, gone) = std_mpsc::channel::<()>();
    let task = runtime.spawn(async move {
        let _served = served;
        listener
            .serve(LaunchContext {
                job: &context.job,
                brain_exe: &context.brain_exe,
                deadline: context.deadline.into(),
                tools: &context.tools,
                gate: context.gate,
                binding: context.binding,
                calls: context.calls,
            })
            .await;
    });
    (task.abort_handle(), gone)
}

/// Everything a launch has made so far, which its teardown ends in order.
struct Running {
    control: Arc<Control>,
    /// Kumochi's folders and the engine folder: held through the deletion.
    parents: Vec<File>,
    /// The launch folder's own handles; they'd stop its deletion.
    held: Vec<File>,
    dir: Option<PathBuf>,
    listener: Option<(tokio::task::AbortHandle, std_mpsc::Receiver<()>)>,
    job: Option<Arc<Job>>,
    /// Claude Code's process: it holds its working folder until it has finished exiting.
    child: Option<platform::Helper>,
    pumps: Vec<std_mpsc::Receiver<()>>,
    #[cfg(test)]
    terminate: Option<tests::Pause>,
}

impl Running {
    /// Fails the gate and closes stdin, seals the held send, aborts the listener and waits up to
    /// [`PIPE_GRACE`] for its task to end, ends the job and confirms it empty, waits for the pumps,
    /// and only then settles the send and, once the child has finished exiting or a further
    /// [`PIPE_GRACE`] has passed, deletes the folder, retrying for one more [`PIPE_GRACE`] while a
    /// handle still holds it. Anything not confirmed ended never falls through to either: its send
    /// is abandoned unsettled, so its sources stay held until its deadline fails it closed.
    fn stop(mut self, host: &Host) -> Teardown {
        self.control.revoke();
        let send = self.control.seal_send();
        if !self.confirm(host) {
            if let Some(send) = send {
                send.abandon();
            }
            return Teardown::Stuck;
        }
        // Nothing the launch started can send any more.
        drop(send);
        // The job reads empty before the child's handles close; a failed wait shows as FolderLeft.
        if let Some(child) = &self.child {
            child.exits_within(PIPE_GRACE).ok();
        }
        drop(self.held);
        let Some(dir) = self.dir.as_deref() else {
            return Teardown::Done;
        };
        // A process the launch started may still be closing its handles, as the bridge may after
        // Claude Code; only a folder that stays held past the grace is left for the sweep.
        let deadline = Instant::now() + PIPE_GRACE;
        loop {
            // Each retry is the same checked walk, by a name drawn at random for this launch in a
            // folder held owner-only throughout, so retrying adds no exposure the first try lacks.
            match platform::remove_tree(dir) {
                Ok(()) => return Teardown::Done,
                Err(platform::Untrusted::Io) => {}
                Err(_) => return Teardown::FolderLeft,
            }
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            if Instant::now() >= deadline {
                return Teardown::FolderLeft;
            }
        }
    }

    /// Whether the listener's task, the job and every pump are confirmed ended.
    fn confirm(&mut self, host: &Host) -> bool {
        // A runtime that's gone dropped the task already, which also counts as ended.
        let served = self.listener.as_ref().is_none_or(|(serve, gone)| {
            serve.abort();
            !matches!(
                gone.recv_timeout(PIPE_GRACE),
                Err(std_mpsc::RecvTimeoutError::Timeout)
            )
        });
        #[cfg(test)]
        tests::pause(self.terminate.take());
        #[cfg(test)]
        tests::pause_picture(&self.control, |hooks| hooks.terminate.take());
        if let Some(job) = &self.job
            && (host.terminate)(job).is_err()
        {
            tracing::warn!("a claude launch's processes didn't end; Claude Code stays closed");
            return false;
        }
        // stdout keeps what the child wrote last until the round stops taking it or the grace
        // runs out.
        if !ended(&self.pumps) {
            self.control.released.store(true, Ordering::Release);
            if !ended(&self.pumps) {
                tracing::warn!("a claude launch's pipe didn't end; Claude Code stays closed");
                return false;
            }
        }
        // A task still running may hold the launch's pipe name.
        if !served {
            tracing::warn!("a claude launch's listener didn't end; Claude Code stays closed");
            return false;
        }
        true
    }
}

/// Whether every pump ended, each given up to [`PIPE_GRACE`].
fn ended(pumps: &[std_mpsc::Receiver<()>]) -> bool {
    pumps.iter().all(|pump| {
        matches!(
            pump.recv_timeout(PIPE_GRACE),
            Err(std_mpsc::RecvTimeoutError::Disconnected) | Ok(())
        )
    })
}

/// Runs `work` on its own thread; the receiver hears it end, however it ends.
fn pump(
    name: &'static str,
    work: impl FnOnce() + Send + 'static,
    #[cfg(test)] (report, fails): (Option<std_mpsc::Sender<&'static str>>, Option<&'static str>),
) -> io::Result<std_mpsc::Receiver<()>> {
    #[cfg(test)]
    if fails == Some(name) {
        return Err(io::Error::other("a pump thread that can't start"));
    }
    let (signal, ended) = std_mpsc::channel::<()>();
    #[cfg(test)]
    let signal = tests::Ending::new(signal, name, report);
    let _thread = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _signal = signal;
            work();
        })?;
    Ok(ended)
}

/// stdin: each chunk is admitted, then written. `End`, `Wake`, a refused admission or a failed
/// write closes the pipe.
fn write_input(
    mut pipe: impl io::Write,
    mut input: mpsc::Receiver<Input>,
    control: &Control,
    #[cfg(test)] hooks: tests::Stdin,
) {
    #[cfg(test)]
    let tests::Stdin {
        mut admit,
        mut write,
        writes,
        entries,
    } = hooks;
    while let Some(chunk) = input.blocking_recv() {
        let (bytes, pictured) = match &chunk {
            Input::Chunk(bytes) => (bytes.as_slice(), None),
            Input::Picture(chunk) => (chunk.bytes.as_slice(), Some(chunk)),
            Input::End | Input::Wake => return,
        };
        #[cfg(test)]
        tests::pause(admit.take());
        #[cfg(test)]
        if pictured.is_some() {
            tests::pause_picture(control, |hooks| hooks.write.take());
        }
        if !control.admit() {
            return;
        }
        #[cfg(test)]
        if let Some(entries) = &entries {
            entries.send(bytes.len()).ok();
        }
        #[cfg(test)]
        tests::pause(write.take());
        let result = match pictured {
            Some(chunk) => write_picture(&mut pipe, control, chunk),
            None => pipe.write_all(bytes).map_err(|_| StdinRefused::Revoked),
        };
        #[cfg(test)]
        if let (Some(writes), Ok(())) = (&writes, &result) {
            writes.send(bytes.len()).ok();
        }
        if let Input::Picture(PictureChunk { first, .. }) = chunk {
            if result.is_err() {
                control.revoke();
            }
            if let Some(first) = first {
                first.send(result).ok();
            }
        }
        if !control.written() || result.is_err() {
            return;
        }
    }
}

fn write_picture(
    pipe: &mut impl io::Write,
    control: &Control,
    chunk: &PictureChunk,
) -> Result<(), StdinRefused> {
    let mut bytes = chunk.bytes.as_slice();
    while !bytes.is_empty() {
        let outcome = {
            let _permit = control.admit_picture(chunk.until)?;
            pipe.write(bytes)
        };
        match outcome {
            Ok(written) if written != 0 => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => return Err(StdinRefused::Revoked),
        }
    }
    Ok(())
}

/// stdout, in chunks of at most [`CHUNK`], until its end. A full channel is retried every
/// [`POLL`], never waited on, and once the round has let go what's read is dropped. The read
/// buffer and every chunk, queued or pending, are zeroed when they drop.
fn read_output(
    mut pipe: impl io::Read,
    output: &mpsc::Sender<Zeroizing<Vec<u8>>>,
    control: &Control,
) {
    let mut buffer = Zeroizing::new(vec![0; CHUNK]);
    let mut wanted = true;
    loop {
        let read = match pipe.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => {
                control.output_closed.store(true, Ordering::Release);
                return;
            }
            Ok(read) => read,
        };
        if !wanted {
            continue;
        }
        let mut chunk = Zeroizing::new(buffer[..read].to_vec());
        loop {
            if control.released.load(Ordering::Acquire) {
                wanted = false;
                break;
            }
            match output.try_send(chunk) {
                Ok(()) => break,
                Err(TrySendError::Closed(_)) => {
                    wanted = false;
                    break;
                }
                Err(TrySendError::Full(back)) => {
                    chunk = back;
                    std::thread::sleep(POLL);
                }
            }
        }
    }
}

/// stderr, read to its end and dropped: nothing Claude Code prints there is kept, and the buffer
/// is zeroed when it drops.
fn drain_stderr(mut pipe: impl io::Read) {
    let mut buffer = Zeroizing::new(vec![0; 8 * 1024]);
    loop {
        match pipe.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// The launch's `mcp.json`: the bridge is the brain's executable with the launch's id, and
/// nothing else. `None` for an executable path that isn't text.
fn mcp_json(brain_exe: &Path, id: LaunchId) -> Option<Vec<u8>> {
    let config = serde_json::json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": brain_exe.to_str()?,
                "args": [crate::MCP_BRIDGE, id.hex()],
            },
        },
    });
    Some(config.to_string().into_bytes())
}

#[cfg(test)]
mod stdin_tests;
#[cfg(test)]
mod teardown_tests;
#[cfg(test)]
pub(crate) mod tests;
