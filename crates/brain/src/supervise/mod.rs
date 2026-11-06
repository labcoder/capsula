//! Killable helper processes, one supervisor per kind: `kumochid --screen` and
//! `kumochid --speech`. See `docs/development/processes.md`.
//!
//! A supervisor is one tokio task that owns its worker process, the pending requests, the request
//! ids and the loss history. Two std threads per worker own the pipe ends. A loss fails everything
//! pending, gives the process two seconds to show it exited, broadcasts that [`Exit`] and then its
//! [`Generation`], and respawns, backing off after the second loss in a minute. The loss is the one
//! recovery boundary: requests during confirmation fail; outside crash backoff, requests after
//! the loss reach the next worker. An exit
//! that isn't seen is fatal: nothing respawns and every handle refuses with
//! [`WorkerLost::ExitUnconfirmed`]. A request gets one reply, or a bounded stream of them ending in
//! the kind's terminal reply.

mod wire;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as blocking};
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use self::wire::{Assembly, Frame};
pub(crate) use self::wire::{read_request, write_message};
use crate::platform::{Helper, Job};

/// Losses are counted over this window for backoff.
const WINDOW: Duration = Duration::from_secs(60);
/// Pending requests, and queued frames per channel [guess].
const QUEUE: usize = 16;
/// Whole replies queued per stream, under the message cap so a reader that falls behind is caught
/// [guess].
const STREAM_QUEUE: usize = 4;
/// Messages per request, the terminal one included [guess].
const MAX_MESSAGES: usize = 16;
/// Serialized reply bytes per stream across all its messages, each still under its kind's cap
/// [guess]. A single request keeps its kind's cap.
const MAX_STREAM_BYTES: usize = 12 * 1024 * 1024;
/// How long a terminated worker gets to show it exited [guess].
const EXIT_CONFIRMATION: Duration = Duration::from_secs(2);

/// One kind of worker: how it's started and what it's sent and answers.
pub(crate) trait Kind: 'static {
    /// The argument that starts `kumochid` as this worker.
    const ARG: &'static str;
    /// Its name in thread names and logs.
    const NAME: &'static str;
    /// The largest request and reply, serialized, before chunking.
    const MAX_REQUEST: usize;
    const MAX_REPLY: usize;
    type Request: Serialize + Send + 'static;
    type Reply: DeserializeOwned + Send + 'static;

    /// Whether `reply` ends its request. Every reply of a kind without streams does.
    fn is_terminal(_reply: &Self::Reply) -> bool {
        true
    }
}

/// Why a request got no reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerLost {
    /// The worker died, broke its protocol or missed a deadline, or the request came while a lost
    /// one was being replaced. Outside crash backoff, work after its loss reaches the next worker.
    Lost,
    /// A lost worker wasn't seen to exit, so nothing replaces it and every request is refused.
    ExitUnconfirmed,
}

/// One worker process of a supervisor: the first is 0 and each respawn the next. A reply and a
/// loss name theirs, so a loss can be matched to the work it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

type Answer<K> = oneshot::Sender<Result<(<K as Kind>::Reply, Generation), WorkerLost>>;

/// Whether a lost worker's process was seen to exit after termination. `WorkerLost` alone never
/// says so: only a confirmed exit proves nothing the worker held survives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Exit {
    pub(crate) generation: Generation,
    pub(crate) confirmed: bool,
}

/// What a supervisor tells every handle.
#[derive(Clone)]
struct Notices {
    losses: broadcast::Sender<Generation>,
    exits: broadcast::Sender<Exit>,
    /// Set when an exit goes unconfirmed, before its loss is broadcast; never cleared.
    fatal: Arc<AtomicBool>,
}

/// The failure a request sees when its supervisor gives no answer.
fn failure(fatal: &AtomicBool) -> WorkerLost {
    if fatal.load(Ordering::Acquire) {
        WorkerLost::ExitUnconfirmed
    } else {
        WorkerLost::Lost
    }
}

/// Requests and loss notifications; dropping the last handle stops the worker.
pub(crate) struct Handle<K: Kind> {
    commands: mpsc::Sender<Command<K>>,
    notices: Notices,
}

impl<K: Kind> Clone for Handle<K> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            notices: self.notices.clone(),
        }
    }
}

impl<K: Kind> std::fmt::Debug for Handle<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("kind", &K::NAME)
            .finish_non_exhaustive()
    }
}

struct Command<K: Kind> {
    body: K::Request,
    until: Instant,
    answer: Destination<K>,
}

/// Where a request's whole replies go.
enum Destination<K: Kind> {
    /// `request`: one terminal reply; an intermediate reply is malformed.
    Single(Answer<K>),
    /// `request_stream`: every reply, while its reader keeps up.
    Stream(mpsc::Sender<(K::Reply, Generation)>),
    /// A stream that closed or fell behind: replies are dropped until the terminal one or the
    /// deadline, so its id stays known and the worker isn't lost for it.
    Discard,
}

impl<K: Kind> Destination<K> {
    fn is_closed(&self) -> bool {
        match self {
            Self::Single(answer) => answer.is_closed(),
            Self::Stream(stages) => stages.is_closed(),
            Self::Discard => true,
        }
    }

    /// Hands on one whole reply and says whether it was the terminal one.
    fn deliver(&mut self, reply: K::Reply, generation: Generation) -> Result<bool, WorkerLost> {
        let terminal = K::is_terminal(&reply);
        if matches!(self, Self::Single(_)) && !terminal {
            return Err(WorkerLost::Lost);
        }
        match std::mem::replace(self, Self::Discard) {
            Self::Single(answer) => complete::<K>(answer, Ok((reply, generation))),
            Self::Stream(stages) => {
                if stages.try_send((reply, generation)).is_ok() {
                    *self = Self::Stream(stages);
                } else {
                    tracing::debug!(kind = K::NAME, "stream closed or behind");
                }
            }
            Self::Discard => {}
        }
        Ok(terminal)
    }

    /// A stream's reader sees its sender close as the loss.
    fn fail(self) {
        if let Self::Single(answer) = self {
            complete::<K>(answer, Err(WorkerLost::Lost));
        }
    }
}

impl<K: Kind> Handle<K> {
    fn new(commands: mpsc::Sender<Command<K>>) -> Self {
        Self {
            commands,
            notices: Notices {
                losses: broadcast::channel(QUEUE).0,
                exits: broadcast::channel(QUEUE).0,
                fatal: Arc::default(),
            },
        }
    }

    /// Subscribes before issuing work. Each lost generation's exit is reported once, just before
    /// its loss.
    #[cfg_attr(not(test), expect(dead_code, reason = "A6b2's release waits on it"))]
    pub(crate) fn exits(&self) -> broadcast::Receiver<Exit> {
        self.notices.exits.subscribe()
    }

    /// Submits one request, answered by `until` or not at all. `until` is absolute: it covers
    /// queueing, a respawn in progress and every chunk. A full queue, backoff or exit confirmation
    /// fails fast, and an unconfirmed exit refuses for good. The reply comes with the generation
    /// of the worker that sent it.
    pub(crate) async fn request(
        &self,
        body: K::Request,
        until: Instant,
    ) -> Result<(K::Reply, Generation), WorkerLost> {
        let lost = || failure(&self.notices.fatal);
        if lost() == WorkerLost::ExitUnconfirmed {
            return Err(WorkerLost::ExitUnconfirmed);
        }
        let (answer, reply) = oneshot::channel();
        self.commands
            .try_send(Command {
                body,
                until,
                answer: Destination::Single(answer),
            })
            .map_err(|_| lost())?;
        tokio::time::timeout_at(until, reply)
            .await
            .map_err(|_| lost())?
            .unwrap_or_else(|_| Err(lost()))
    }

    /// Submits one streamed request under the same absolute `until`, which no reply extends.
    /// Dropping the stream doesn't lose the worker: its later replies are discarded.
    ///
    /// # Errors
    /// A full queue fails fast.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "A6b's capture stream is the first caller")
    )]
    pub(crate) fn request_stream(
        &self,
        body: K::Request,
        until: Instant,
    ) -> Result<ReplyStream<K>, WorkerLost> {
        let fatal = Arc::clone(&self.notices.fatal);
        if failure(&fatal) == WorkerLost::ExitUnconfirmed {
            return Err(WorkerLost::ExitUnconfirmed);
        }
        let (stages, replies) = mpsc::channel(STREAM_QUEUE);
        self.commands
            .try_send(Command {
                body,
                until,
                answer: Destination::Stream(stages),
            })
            .map_err(|_| failure(&fatal))?;
        Ok(ReplyStream {
            replies,
            until,
            done: false,
            fatal,
        })
    }

    /// Subscribes before issuing work, so a lost generation can't be missed. Each loss names the
    /// generation that ended, after every request it held has been answered.
    pub(crate) fn lost(&self) -> broadcast::Receiver<Generation> {
        self.notices.losses.subscribe()
    }
}

/// A streamed request's replies, in order. Debug prints no reply.
// shortcut: dropping it tells the worker nothing; A6b's capture stream sends Drop.
pub(crate) struct ReplyStream<K: Kind> {
    replies: mpsc::Receiver<(K::Reply, Generation)>,
    until: Instant,
    done: bool,
    fatal: Arc<AtomicBool>,
}

impl<K: Kind> ReplyStream<K> {
    /// The next reply, then `None` after the terminal one or `WorkerLost`. A loss, the deadline
    /// or falling behind ends the stream with `WorkerLost`, after every reply already queued.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "A6b's capture stream is the first caller")
    )]
    pub(crate) async fn next(&mut self) -> Option<Result<(K::Reply, Generation), WorkerLost>> {
        if self.done {
            return None;
        }
        let next = tokio::time::timeout_at(self.until, self.replies.recv())
            .await
            .ok()
            .flatten();
        self.done = next.as_ref().is_none_or(|(reply, _)| K::is_terminal(reply));
        Some(next.ok_or_else(|| failure(&self.fatal)))
    }
}

impl<K: Kind> std::fmt::Debug for ReplyStream<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyStream")
            .field("kind", &K::NAME)
            .field("queued", &self.replies.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// Starts a `K` worker directly in `jobs` and its supervisor task.
///
/// # Errors
/// Returns a process or thread creation failure.
pub(crate) fn start<K: Kind>(exe: PathBuf, jobs: Arc<Job>) -> io::Result<Handle<K>> {
    let worker = Worker::<K>::spawn(&exe, &jobs)?;
    let (commands, receive) = mpsc::channel(QUEUE);
    let handle = Handle::new(commands);
    drop(tokio::spawn(run(
        move || Worker::<K>::spawn(&exe, &jobs),
        worker,
        receive,
        handle.notices.clone(),
    )));
    Ok(handle)
}

/// Each request a test plays the worker for: its body and the answer to send.
#[cfg(test)]
type Played<K> = mpsc::Receiver<(<K as Kind>::Request, Answer<K>)>;

/// A handle with no worker: each request's body and answer arrive on the returned queue, so a
/// kind's own tests can play the worker.
#[cfg(test)]
pub(crate) fn detached<K: Kind>() -> (Handle<K>, Played<K>) {
    let (commands, mut receive) = mpsc::channel::<Command<K>>(QUEUE);
    let (forward, requests) = mpsc::channel(QUEUE);
    drop(tokio::spawn(async move {
        while let Some(command) = receive.recv().await {
            // shortcut: a stream is dropped here, so it ends lost; A6b's fake worker plays streams.
            let Destination::Single(answer) = command.answer else {
                continue;
            };
            if forward.send((command.body, answer)).await.is_err() {
                return;
            }
        }
    }));
    (Handle::new(commands), requests)
}

struct Worker<K: Kind> {
    /// Taken for exit confirmation once the worker is lost.
    process: Option<Helper>,
    writes: blocking::SyncSender<(u64, K::Request)>,
    reads: mpsc::Receiver<Option<Frame>>,
}

impl<K: Kind> Worker<K> {
    fn spawn(exe: &Path, job: &Job) -> io::Result<Self> {
        let (process, mut pipes) = Job::spawn_piped(exe, K::ARG, &[job])?;
        let (writes, requests) = blocking::sync_channel::<(u64, K::Request)>(QUEUE);
        let (replies, reads) = mpsc::channel(QUEUE);
        let worker = Self {
            process: Some(process),
            writes,
            reads,
        };
        let errors = replies.clone();
        drop(
            std::thread::Builder::new()
                .name(format!("{}-reader", K::NAME))
                .spawn(move || {
                    loop {
                        let reply = frame::read(&mut pipes.stdout, Pipe::Data).ok().flatten();
                        let ended = reply.is_none();
                        if replies.blocking_send(reply).is_err() || ended {
                            break;
                        }
                    }
                })?,
        );
        drop(
            std::thread::Builder::new()
                .name(format!("{}-writer", K::NAME))
                .spawn(move || {
                    while let Ok((id, body)) = requests.recv() {
                        if wire::write_message(&mut pipes.stdin, id, &body, K::MAX_REQUEST).is_err()
                        {
                            if errors.blocking_send(None).is_err() {
                                return;
                            }
                            break;
                        }
                    }
                })?,
        );
        Ok(worker)
    }
}

impl<K: Kind> Drop for Worker<K> {
    fn drop(&mut self) {
        terminate::<K>(self.process.as_ref());
    }
}

/// Asks the OS to end the worker's process; only an exit wait shows it ended.
fn terminate<K: Kind>(process: Option<&Helper>) {
    if let Some(Err(error)) = process.map(Helper::terminate) {
        tracing::error!(
            kind = K::NAME,
            code = error.raw_os_error(),
            "worker termination failed"
        );
    }
}

/// Waits off the runtime thread for `exited`, given [`EXIT_CONFIRMATION`], failing commands that
/// arrive meanwhile, then broadcasts `generation`'s [`Exit`] and then its loss. An unconfirmed exit
/// is made fatal first, so whoever the loss wakes is refused. Returns whether the exit was seen.
async fn confirm_exit<K: Kind>(
    exited: impl FnOnce(Duration) -> bool + Send + 'static,
    generation: Generation,
    commands: &mut mpsc::Receiver<Command<K>>,
    notices: &Notices,
) -> bool {
    let mut waiting = tokio::task::spawn_blocking(move || exited(EXIT_CONFIRMATION));
    let confirmed = loop {
        tokio::select! {
            biased;
            exited = &mut waiting => break exited.unwrap_or(false),
            Some(command) = commands.recv() => command.answer.fail(),
        }
    };
    if !confirmed {
        notices.fatal.store(true, Ordering::Release);
        tracing::error!(
            kind = K::NAME,
            "worker exit unconfirmed, so it is not replaced"
        );
    }
    if notices
        .exits
        .send(Exit {
            generation,
            confirmed,
        })
        .is_err()
    {
        tracing::debug!(kind = K::NAME, "worker exit without watchers");
    }
    if notices.losses.send(generation).is_err() {
        tracing::debug!(kind = K::NAME, "worker lost without watchers");
    }
    confirmed
}

/// Why a worker generation ended; content-free, for the log only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LossReason {
    /// A request passed its deadline.
    Deadline,
    /// A reply broke the wire contract.
    Malformed,
    /// The worker's stdout ended or failed, or a request couldn't be written.
    Ended,
    /// A request couldn't reach the writer thread.
    Unwritable,
}

struct Pending<K: Kind> {
    until: Instant,
    answer: Destination<K>,
    assembly: Assembly,
    /// What the request may still receive: whole messages, and serialized bytes across them.
    messages: usize,
    bytes: usize,
}

impl<K: Kind> Pending<K> {
    fn new(until: Instant, answer: Destination<K>) -> Self {
        let bytes = match answer {
            Destination::Single(_) => K::MAX_REPLY,
            Destination::Stream(_) | Destination::Discard => MAX_STREAM_BYTES,
        };
        Self {
            until,
            answer,
            assembly: Assembly::new(K::MAX_REPLY),
            messages: MAX_MESSAGES,
            bytes,
        }
    }
}

fn complete<K: Kind>(answer: Answer<K>, reply: Result<(K::Reply, Generation), WorkerLost>) {
    if answer.send(reply).is_err() {
        tracing::debug!(kind = K::NAME, "requester cancelled");
    }
}

/// Adds one reply frame to its pending request, delivering each whole message and ending the
/// request on its terminal one. A frame for no pending request, one that breaks the chunk rules,
/// or one past the request's message or byte budget is a protocol break.
fn accept<K: Kind>(
    pending: &mut BTreeMap<u64, Pending<K>>,
    reply: Frame,
    generation: Generation,
) -> Result<(), WorkerLost> {
    let id = reply.id;
    let item = pending.get_mut(&id).ok_or(WorkerLost::Lost)?;
    item.bytes = item
        .bytes
        .checked_sub(reply.body.len())
        .ok_or(WorkerLost::Lost)?;
    let Some(bytes) = item.assembly.push(reply)? else {
        return Ok(());
    };
    item.messages = item.messages.checked_sub(1).ok_or(WorkerLost::Lost)?;
    let body = frame::decode::<K::Reply>(&bytes).map_err(|_| WorkerLost::Lost)?;
    if item.answer.deliver(body, generation)? {
        pending.remove(&id);
    }
    Ok(())
}

fn backoff(losses: &mut VecDeque<Instant>, now: Instant) -> Duration {
    while losses
        .front()
        .is_some_and(|at| now.duration_since(*at) >= WINDOW)
    {
        losses.pop_front();
    }
    losses.push_back(now);
    match losses.len() {
        0..=2 => Duration::ZERO,
        3 => Duration::from_millis(200),
        4 => Duration::from_secs(1),
        _ => Duration::from_secs(5),
    }
}

/// Serves one worker until it's lost, then fails everything it held. `None` once every handle is
/// gone or the request ids run out.
async fn serve<K: Kind>(
    reads: &mut mpsc::Receiver<Option<Frame>>,
    writes: &blocking::SyncSender<(u64, K::Request)>,
    commands: &mut mpsc::Receiver<Command<K>>,
    id: &mut u64,
    generation: Generation,
) -> Option<LossReason> {
    let mut pending: BTreeMap<u64, Pending<K>> = BTreeMap::new();
    let reason = loop {
        let until = pending
            .values()
            .map(|p| p.until)
            .min()
            .unwrap_or_else(|| Instant::now() + WINDOW);
        let lost = tokio::select! {
            biased;
            () = tokio::time::sleep_until(until), if !pending.is_empty() => Some(LossReason::Deadline),
            reply = reads.recv() => match reply.flatten() {
                Some(reply) => accept(&mut pending, reply, generation).err().map(|_| LossReason::Malformed),
                None => Some(LossReason::Ended),
            },
            command = commands.recv() => {
                let command = command?;
                if command.answer.is_closed() { continue; }
                if pending.len() == QUEUE || command.until <= Instant::now() { command.answer.fail(); continue; }
                *id = id.checked_add(1)?;
                pending.insert(*id, Pending::new(command.until, command.answer));
                writes.try_send((*id, command.body)).err().map(|_| LossReason::Unwritable)
            }
        };
        if let Some(reason) = lost {
            break reason;
        }
    };
    for (_, item) in pending {
        item.answer.fail();
    }
    Some(reason)
}

async fn run<K: Kind>(
    spawn: impl Fn() -> io::Result<Worker<K>> + Clone + Send + 'static,
    mut worker: Worker<K>,
    mut commands: mpsc::Receiver<Command<K>>,
    notices: Notices,
) {
    let mut generation = Generation(0);
    let mut history = VecDeque::new();
    let mut id = 0_u64;
    loop {
        let Some(reason) = serve(
            &mut worker.reads,
            &worker.writes,
            &mut commands,
            &mut id,
            generation,
        )
        .await
        else {
            return;
        };
        tracing::warn!(kind = K::NAME, ?reason, "worker lost");
        let process = worker.process.take();
        drop(worker);
        terminate::<K>(process.as_ref());
        while let Ok(command) = commands.try_recv() {
            command.answer.fail();
        }
        let exited = move |timeout| {
            process.is_some_and(|process| matches!(process.exits_within(timeout), Ok(true)))
        };
        let confirmed = confirm_exit(exited, generation, &mut commands, &notices).await;
        // A worker that may still run is never joined by another. With every handle gone, a
        // respawn would only land in a closing job.
        if !confirmed || commands.is_closed() {
            return;
        }
        loop {
            let delay = backoff(&mut history, Instant::now());
            let resume = Instant::now() + delay;
            while !delay.is_zero() {
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(resume) => break,
                    command = commands.recv() => match command {
                        Some(command) => command.answer.fail(),
                        None => return,
                    },
                }
            }
            match tokio::task::spawn_blocking(spawn.clone()).await {
                Ok(Ok(next)) => {
                    tracing::warn!(
                        kind = K::NAME,
                        delay_ms = delay.as_millis(),
                        "worker respawned"
                    );
                    worker = next;
                    generation = Generation(generation.0 + 1);
                    break;
                }
                Ok(Err(error)) => {
                    tracing::error!(
                        kind = K::NAME,
                        code = error.raw_os_error(),
                        "worker spawn failed"
                    );
                }
                Err(_) => tracing::error!(kind = K::NAME, code = 2, "worker spawn task failed"),
            }
            while let Ok(command) = commands.try_recv() {
                command.answer.fail();
            }
        }
    }
}
