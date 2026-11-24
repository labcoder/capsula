//! Killable helper processes, one supervisor per kind: `kumochid --screen` and
//! `kumochid --speech`. See `docs/development/processes.md`.
//!
//! A supervisor is one tokio task that owns its worker process, the pending requests, the request
//! ids and the loss history. Two std threads per worker own the pipe ends. A loss fails everything
//! pending, gives the process two seconds to show it exited, broadcasts that [`Exit`] and then its
//! [`Generation`], and respawns, backing off after the second loss in a minute. The loss is the one
//! recovery boundary: requests that arrive during the exit wait are held, as during a respawn, and
//! outside crash backoff they and requests after the loss reach the next worker. An exit that
//! isn't seen is fatal, a replacement's whose threads didn't start included: nothing respawns and
//! every handle, held requests included, is refused with [`WorkerLost::ExitUnconfirmed`]. A
//! request gets one reply, or a bounded stream of them ending in the kind's terminal reply.

mod resource;
mod wire;

#[cfg(test)]
pub(crate) mod fixture;

pub(crate) use resource::{Completion, Retirement};

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

use kumochi_protocol::frame;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

use self::wire::Frame;
#[cfg(any(windows, test))]
pub(crate) use self::wire::write_message;
pub(crate) use self::wire::{Assembly, Reassembly, Zeroed, read_request, write_sized};
use crate::platform::{Helper, Job};

/// Losses are counted over this window for backoff.
const WINDOW: Duration = Duration::from_secs(60);
/// Pending requests, and queued frames per channel [guess].
pub(crate) const QUEUE: usize = 16;
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
    /// How its replies are read and put back together: [`Zeroed`] when they can carry the
    /// user's content.
    type Assembly: Reassembly;

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

/// Whether one command could have reached a worker, independent of its caller's deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    Pending,
    NotDispatched,
    Dispatched(Generation),
}

/// Only a command owns this after admission to the queue. Dropping an unhanded command
/// definitively discards its body; dropping the caller's future does neither.
struct Dispatching(Option<watch::Sender<Dispatch>>);

impl Dispatching {
    fn dispatched(&self, generation: Generation) {
        if let Some(dispatch) = &self.0 {
            dispatch.send_if_modified(|state| {
                if *state != Dispatch::Pending {
                    return false;
                }
                *state = Dispatch::Dispatched(generation);
                true
            });
        }
    }
}

impl Drop for Dispatching {
    fn drop(&mut self) {
        if let Some(dispatch) = &self.0 {
            dispatch.send_if_modified(|state| {
                if *state != Dispatch::Pending {
                    return false;
                }
                *state = Dispatch::NotDispatched;
                true
            });
        }
    }
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
    retirement: Retirement,
}

impl<K: Kind> Clone for Handle<K> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            notices: self.notices.clone(),
            retirement: self.retirement.clone(),
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
    Resource(Box<resource::Destination<K::Reply>>),
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
            Self::Resource(answer) => answer.is_closed(),
            Self::Stream(stages) => stages.is_closed(),
            Self::Discard => true,
        }
    }

    /// Hands on one whole reply and says whether it was the terminal one.
    fn deliver(&mut self, reply: K::Reply, generation: Generation) -> Result<bool, WorkerLost> {
        let terminal = K::is_terminal(&reply);
        if matches!(self, Self::Single(_) | Self::Resource(_)) && !terminal {
            return Err(WorkerLost::Lost);
        }
        match std::mem::replace(self, Self::Discard) {
            Self::Single(answer) => complete::<K>(answer, Ok((reply, generation))),
            Self::Resource(answer) => answer.deliver(reply, generation),
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
        match self {
            Self::Single(answer) => complete::<K>(answer, Err(WorkerLost::Lost)),
            Self::Resource(answer) => answer.fail(),
            Self::Stream(_) | Self::Discard => {}
        }
    }

    fn matches(&self, generation: Generation) -> bool {
        match self {
            Self::Resource(answer) => answer.matches(generation),
            _ => true,
        }
    }

    fn dispatched(&mut self, generation: Generation) {
        if let Self::Resource(answer) = self {
            answer.dispatched(generation);
        }
    }
}

impl<K: Kind> Handle<K> {
    fn new(commands: mpsc::Sender<Command<K>>) -> (Self, resource::Intents) {
        let (retirement, intents) = Retirement::channel();
        (
            Self {
                commands,
                retirement,
                notices: Notices {
                    losses: broadcast::channel(QUEUE).0,
                    exits: broadcast::channel(QUEUE).0,
                    fatal: Arc::default(),
                },
            },
            intents,
        )
    }

    pub(crate) fn retirement(&self) -> Retirement {
        self.retirement.clone()
    }

    /// Subscribes before issuing work. Each lost generation's exit is reported once, just before
    /// its loss.
    pub(crate) fn exits(&self) -> broadcast::Receiver<Exit> {
        self.notices.exits.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn test_exits(&self) -> broadcast::Sender<Exit> {
        self.notices.exits.clone()
    }

    /// Makes the supervisor fatal as an unconfirmed exit does, without publishing that exit.
    #[cfg(test)]
    pub(crate) fn test_fatal(&self) {
        self.notices.fatal.store(true, Ordering::Release);
    }

    /// Submits one request, answered by `until` or not at all. `until` is absolute: it covers
    /// queueing, an exit confirmation or respawn in progress and every chunk. A full queue or
    /// backoff fails fast, and an unconfirmed exit refuses for good. The reply comes with the
    /// generation of the worker that sent it.
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

    /// Tracks actual dispatch after the reply deadline too. Only the supervisor's definitive
    /// discard permits `NotDispatched`; a closed receiver still showing `Pending` proves nothing.
    #[cfg(test)]
    pub(crate) async fn request_tracked(
        &self,
        body: K::Request,
        until: Instant,
        dispatch: watch::Sender<Dispatch>,
    ) -> Result<(K::Reply, Generation), WorkerLost> {
        self.request_resource(body, until, None, Some(dispatch))
            .await?
            .handoff()
    }

    pub(crate) fn request_resource(
        &self,
        body: K::Request,
        until: Instant,
        expected: Option<Generation>,
        dispatch: Option<watch::Sender<Dispatch>>,
    ) -> impl std::future::Future<Output = Result<Completion<K::Reply>, WorkerLost>> + '_ {
        let (answer, reply) =
            resource::Destination::new(expected, until, self.retirement(), dispatch);
        async move {
            let lost = || failure(&self.notices.fatal);
            if lost() == WorkerLost::ExitUnconfirmed {
                return Err(WorkerLost::ExitUnconfirmed);
            }
            self.commands
                .try_send(Command {
                    body,
                    until,
                    answer: Destination::Resource(Box::new(answer)),
                })
                .map_err(|_| lost())?;
            tokio::time::timeout_at(until, reply)
                .await
                .map_err(|_| lost())?
                .unwrap_or_else(|_| Err(lost()))
        }
    }

    /// Submits one streamed request under the same absolute `until`, which no reply extends.
    /// Dropping the stream doesn't lose the worker: its later replies are discarded.
    ///
    /// # Errors
    /// A full queue fails fast.
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
/// Returns a process or thread creation failure, or that the process left by a thread creation
/// failure wasn't seen to exit.
pub(crate) fn start<K: Kind>(exe: PathBuf, jobs: Arc<Job>) -> io::Result<Handle<K>> {
    let worker = Worker::<K>::spawn(&exe, &jobs).map_err(|failure| match failure {
        SpawnFailure::Failed(error) => error,
        SpawnFailure::ExitUnconfirmed => io::Error::other("worker exit unconfirmed"),
        SpawnFailure::ShutDown => io::Error::other("the job was shut down"),
    })?;
    let (commands, receive) = mpsc::channel(QUEUE);
    let (handle, intents) = Handle::new(commands);
    drop(tokio::spawn(run(
        move || Worker::<K>::spawn(&exe, &jobs),
        worker,
        receive,
        handle.notices.clone(),
        intents,
        Generation(0),
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
    let (handle, intents) = Handle::new(commands);
    drop(tokio::spawn(async move {
        let _intents = intents;
        while let Some(mut command) = receive.recv().await {
            // shortcut: a stream is dropped here, so it ends lost; A6b's fake worker plays streams.
            match command.answer {
                Destination::Single(answer) => {
                    if forward.send((command.body, answer)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Destination::Resource(ref mut answer) => answer.dispatched(Generation(0)),
                Destination::Stream(_) | Destination::Discard => continue,
            }
            let (answer, reply) = oneshot::channel();
            if forward.send((command.body, answer)).await.is_err() {
                return;
            }
            drop(tokio::spawn(async move {
                match reply.await {
                    Ok(Ok((reply, generation))) => {
                        if command.answer.deliver(reply, generation).is_err() {
                            tracing::debug!("malformed detached reply");
                        }
                    }
                    _ => command.answer.fail(),
                }
            }));
        }
    }));
    (handle, requests)
}

struct Worker<K: Kind> {
    /// Taken for exit confirmation once the worker is lost.
    process: Option<Box<dyn Process>>,
    writes: blocking::SyncSender<(u64, K::Request)>,
    reads: mpsc::Receiver<Option<Chunk<K>>>,
}

/// A reply frame as a kind's reassembly holds it.
type Chunk<K> = Frame<<<K as Kind>::Assembly as Reassembly>::Body>;

/// Why a worker didn't start.
enum SpawnFailure {
    /// Nothing it started still runs.
    Failed(io::Error),
    /// Its process started and wasn't seen to exit after termination.
    ExitUnconfirmed,
    /// Its job was shut down, so nothing started.
    ShutDown,
}

impl<K: Kind> Worker<K> {
    fn spawn(exe: &Path, job: &Job) -> Result<Self, SpawnFailure> {
        let (process, mut pipes) = Job::spawn_piped(exe, K::ARG, &[job]).map_err(|error| {
            if job.is_shut_down() {
                SpawnFailure::ShutDown
            } else {
                SpawnFailure::Failed(error)
            }
        })?;
        let (writes, requests) = blocking::sync_channel::<(u64, K::Request)>(QUEUE);
        let (replies, reads) = mpsc::channel(QUEUE);
        let mut worker = Self {
            process: Some(Box::new(process)),
            writes,
            reads,
        };
        let errors = replies.clone();
        drop(
            std::thread::Builder::new()
                .name(format!("{}-reader", K::NAME))
                .spawn(move || {
                    loop {
                        let reply = K::Assembly::read(&mut pipes.stdout).ok().flatten();
                        let ended = reply.is_none();
                        if replies.blocking_send(reply).is_err() || ended {
                            break;
                        }
                    }
                })
                .map_err(|error| worker.abandon(error))?,
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
                })
                .map_err(|error| worker.abandon(error))?,
        );
        Ok(worker)
    }

    /// Ends the process of a worker whose threads didn't start and waits for its exit, so a
    /// respawn never runs beside it.
    fn abandon(&mut self, error: io::Error) -> SpawnFailure {
        let process = self.process.take();
        terminate::<K>(process.as_deref());
        if process
            .is_some_and(|process| matches!(process.exits_within(EXIT_CONFIRMATION), Ok(true)))
        {
            SpawnFailure::Failed(error)
        } else {
            SpawnFailure::ExitUnconfirmed
        }
    }
}

impl<K: Kind> Drop for Worker<K> {
    fn drop(&mut self) {
        terminate::<K>(self.process.as_deref());
    }
}

/// A worker's process, as its supervisor ends it.
trait Process: Send {
    fn terminate(&self) -> io::Result<()>;
    fn exits_within(&self, timeout: Duration) -> io::Result<bool>;
}

impl Process for Helper {
    fn terminate(&self) -> io::Result<()> {
        Helper::terminate(self)
    }

    fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        Helper::exits_within(self, timeout)
    }
}

/// Asks the OS to end the worker's process; only an exit wait shows it ended.
fn terminate<K: Kind>(process: Option<&dyn Process>) {
    if let Some(Err(error)) = process.map(Process::terminate) {
        tracing::error!(
            kind = K::NAME,
            code = error.raw_os_error(),
            "worker termination failed"
        );
    }
}

/// Waits off the runtime thread for `exited`, given [`EXIT_CONFIRMATION`], while commands that
/// arrive wait in the queue as they do during a respawn. Then broadcasts `generation`'s [`Exit`]
/// and then its loss. An unconfirmed exit is made fatal first, so whoever the loss wakes, and
/// every held command, is refused. Returns whether the exit was seen.
async fn confirm_exit<K: Kind>(
    exited: impl FnOnce(Duration) -> bool + Send + 'static,
    generation: Generation,
    notices: &Notices,
) -> bool {
    let confirmed = tokio::task::spawn_blocking(move || exited(EXIT_CONFIRMATION))
        .await
        .unwrap_or(false);
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
    Retired,
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
    assembly: K::Assembly,
    /// What the request may still receive: whole messages, and serialized bytes across them.
    messages: usize,
    bytes: usize,
}

impl<K: Kind> Pending<K> {
    fn new(until: Instant, answer: Destination<K>) -> Self {
        let bytes = match answer {
            Destination::Single(_) | Destination::Resource(_) => K::MAX_REPLY,
            Destination::Stream(_) | Destination::Discard => MAX_STREAM_BYTES,
        };
        Self {
            until,
            answer,
            assembly: K::Assembly::new(K::MAX_REPLY),
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
    reply: Chunk<K>,
    generation: Generation,
) -> Result<(), WorkerLost> {
    let id = reply.id;
    let item = pending.get_mut(&id).ok_or(WorkerLost::Lost)?;
    item.bytes = item
        .bytes
        .checked_sub(reply.body.as_ref().len())
        .ok_or(WorkerLost::Lost)?;
    let Some(body) = item.assembly.push(reply, frame::decode::<K::Reply>)? else {
        return Ok(());
    };
    item.messages = item.messages.checked_sub(1).ok_or(WorkerLost::Lost)?;
    let body = body.map_err(|_| WorkerLost::Lost)?;
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
    reads: &mut mpsc::Receiver<Option<Chunk<K>>>,
    writes: &blocking::SyncSender<(u64, K::Request)>,
    commands: &mut mpsc::Receiver<Command<K>>,
    id: &mut u64,
    generation: Generation,
    intents: &mut resource::Intents,
) -> Option<LossReason> {
    let mut pending: BTreeMap<u64, Pending<K>> = BTreeMap::new();
    let mut retirement_open = true;
    let reason = loop {
        if commands.is_closed() {
            break None;
        }
        if *intents.borrow_and_update() == Some(generation) {
            break Some(LossReason::Retired);
        }
        let until = pending
            .values()
            .map(|p| p.until)
            .min()
            .unwrap_or_else(|| Instant::now() + WINDOW);
        let lost = tokio::select! {
            biased;
            () = tokio::time::sleep_until(until), if !pending.is_empty() => Some(LossReason::Deadline),
            changed = intents.changed(), if retirement_open => {
                retirement_open = changed.is_ok();
                None
            },
            reply = reads.recv() => match reply.flatten() {
                Some(reply) => accept(&mut pending, reply, generation).err().map(|_| LossReason::Malformed),
                None => Some(LossReason::Ended),
            },
            command = commands.recv() => {
                let Some(mut command) = command else { break None; };
                if command.answer.is_closed() { continue; }
                if pending.len() == QUEUE || command.until <= Instant::now() || !command.answer.matches(generation) { command.answer.fail(); continue; }
                let Some(next) = id.checked_add(1) else { break None; };
                *id = next;
                command.answer.dispatched(generation);
                pending.insert(*id, Pending::new(command.until, command.answer));
                writes.try_send((*id, command.body)).err().map(|_| LossReason::Unwritable)
            }
        };
        if let Some(reason) = lost {
            break Some(reason);
        }
    };
    for (_, item) in pending {
        item.answer.fail();
    }
    reason
}

async fn run<K: Kind>(
    spawn: impl Fn() -> Result<Worker<K>, SpawnFailure> + Clone + Send + 'static,
    mut worker: Worker<K>,
    mut commands: mpsc::Receiver<Command<K>>,
    notices: Notices,
    mut intents: resource::Intents,
    mut generation: Generation,
) {
    let mut history = VecDeque::new();
    let mut id = 0_u64;
    loop {
        let reason = serve(
            &mut worker.reads,
            &worker.writes,
            &mut commands,
            &mut id,
            generation,
            &mut intents,
        )
        .await;
        if let Some(reason) = reason {
            tracing::warn!(kind = K::NAME, ?reason, "worker lost");
        } else {
            tracing::debug!(kind = K::NAME, "worker stopped");
        }
        let process = worker.process.take();
        drop(worker);
        terminate::<K>(process.as_deref());
        while let Ok(command) = commands.try_recv() {
            command.answer.fail();
        }
        let exited = move |timeout| {
            process.is_some_and(|process| matches!(process.exits_within(timeout), Ok(true)))
        };
        let confirmed = confirm_exit::<K>(exited, generation, &notices).await;
        // A worker that may still run is never joined by another. With every handle gone, a
        // respawn would only land in a closing job.
        if !confirmed || commands.is_closed() || reason.is_none() {
            return;
        }
        let Some(next_generation) = generation.0.checked_add(1).map(Generation) else {
            return;
        };
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
                    generation = next_generation;
                    break;
                }
                Ok(Err(SpawnFailure::ExitUnconfirmed)) => {
                    notices.fatal.store(true, Ordering::Release);
                    tracing::error!(
                        kind = K::NAME,
                        "worker exit unconfirmed, so it is not replaced"
                    );
                    return;
                }
                Ok(Err(SpawnFailure::ShutDown)) => {
                    tracing::debug!(kind = K::NAME, "worker not replaced: its job is shut down");
                    return;
                }
                Ok(Err(SpawnFailure::Failed(error))) => {
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
