//! Killable helper processes, one supervisor per kind: `kumochid --screen` and
//! `kumochid --speech`. See `docs/development/processes.md`.
//!
//! A supervisor is one tokio task that owns its worker process, the pending requests, the request
//! ids and the loss history. Two std threads per worker own the pipe ends. A loss terminates the
//! worker, fails everything pending, broadcasts its [`Generation`] and respawns, backing off after
//! the second loss in a minute.

mod wire;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
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
}

/// The worker died, broke its protocol or missed a deadline; everything it held is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerLost;

/// One worker process of a supervisor: the first is 0 and each respawn the next. A reply and a
/// loss name theirs, so a loss can be matched to the work it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

type Answer<K> = oneshot::Sender<Result<(<K as Kind>::Reply, Generation), WorkerLost>>;

/// Requests and loss notifications; dropping the last handle stops the worker.
pub(crate) struct Handle<K: Kind> {
    commands: mpsc::Sender<Command<K>>,
    losses: broadcast::Sender<Generation>,
}

impl<K: Kind> Clone for Handle<K> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            losses: self.losses.clone(),
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
    answer: Answer<K>,
}

impl<K: Kind> Handle<K> {
    /// Submits one request, answered by `until` or not at all. `until` is absolute: it covers
    /// queueing, a respawn in progress and every chunk. A full queue or backoff fails fast. The
    /// reply comes with the generation of the worker that sent it.
    pub(crate) async fn request(
        &self,
        body: K::Request,
        until: Instant,
    ) -> Result<(K::Reply, Generation), WorkerLost> {
        let (answer, reply) = oneshot::channel();
        self.commands
            .try_send(Command {
                body,
                until,
                answer,
            })
            .map_err(|_| WorkerLost)?;
        tokio::time::timeout_at(until, reply)
            .await
            .map_err(|_| WorkerLost)?
            .unwrap_or(Err(WorkerLost))
    }

    /// Subscribes before issuing work, so a lost generation can't be missed. Each loss names the
    /// generation that ended, after every request it held has been answered.
    pub(crate) fn lost(&self) -> broadcast::Receiver<Generation> {
        self.losses.subscribe()
    }
}

/// Starts a `K` worker directly in `jobs` and its supervisor task.
///
/// # Errors
/// Returns a process or thread creation failure.
pub(crate) fn start<K: Kind>(exe: PathBuf, jobs: Arc<Job>) -> io::Result<Handle<K>> {
    let worker = Worker::<K>::spawn(&exe, &jobs)?;
    let (commands, receive) = mpsc::channel(QUEUE);
    let (losses, _) = broadcast::channel(QUEUE);
    drop(tokio::spawn(run(
        exe,
        jobs,
        worker,
        receive,
        losses.clone(),
    )));
    Ok(Handle { commands, losses })
}

/// Each request a test plays the worker for: its body and the answer to send.
#[cfg(test)]
type Played<K> = mpsc::Receiver<(<K as Kind>::Request, Answer<K>)>;

/// A handle with no worker: each request's body and answer arrive on the returned queue, so a
/// kind's own tests can play the worker.
#[cfg(test)]
pub(crate) fn detached<K: Kind>() -> (Handle<K>, Played<K>) {
    let (commands, mut receive) = mpsc::channel::<Command<K>>(QUEUE);
    let (losses, _) = broadcast::channel(QUEUE);
    let (forward, requests) = mpsc::channel(QUEUE);
    drop(tokio::spawn(async move {
        while let Some(command) = receive.recv().await {
            if forward.send((command.body, command.answer)).await.is_err() {
                return;
            }
        }
    }));
    (Handle { commands, losses }, requests)
}

struct Worker<K: Kind> {
    process: Helper,
    writes: blocking::SyncSender<(u64, K::Request)>,
    reads: mpsc::Receiver<Option<Frame>>,
}

impl<K: Kind> Worker<K> {
    fn spawn(exe: &Path, job: &Job) -> io::Result<Self> {
        let (process, mut pipes) = Job::spawn_piped(exe, K::ARG, &[job])?;
        let (writes, requests) = blocking::sync_channel::<(u64, K::Request)>(QUEUE);
        let (replies, reads) = mpsc::channel(QUEUE);
        let worker = Self {
            process,
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
        if let Err(error) = self.process.terminate() {
            tracing::error!(
                kind = K::NAME,
                code = error.raw_os_error(),
                "worker termination failed"
            );
        }
    }
}

/// Why a worker generation ended; content-free, for the log only.
#[derive(Clone, Copy, Debug)]
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
    answer: Answer<K>,
    assembly: Assembly,
}

fn complete<K: Kind>(answer: Answer<K>, reply: Result<(K::Reply, Generation), WorkerLost>) {
    if answer.send(reply).is_err() {
        tracing::debug!(kind = K::NAME, "requester cancelled");
    }
}

/// Adds one reply frame to its pending request, completing it on the last chunk. A frame for no
/// pending request, or one that breaks the chunk rules, is a protocol break.
fn accept<K: Kind>(
    pending: &mut BTreeMap<u64, Pending<K>>,
    reply: Frame,
    generation: Generation,
) -> Result<(), WorkerLost> {
    let id = reply.id;
    let item = pending.get_mut(&id).ok_or(WorkerLost)?;
    if let Some(bytes) = item.assembly.push(reply)? {
        let body = frame::decode::<K::Reply>(&bytes).map_err(|_| WorkerLost)?;
        let item = pending.remove(&id).ok_or(WorkerLost)?;
        complete::<K>(item.answer, Ok((body, generation)));
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

async fn run<K: Kind>(
    exe: PathBuf,
    jobs: Arc<Job>,
    mut worker: Worker<K>,
    mut commands: mpsc::Receiver<Command<K>>,
    losses: broadcast::Sender<Generation>,
) {
    let mut generation = Generation(0);
    let mut history = VecDeque::new();
    let mut pending: BTreeMap<u64, Pending<K>> = BTreeMap::new();
    let mut id = 0_u64;
    loop {
        let until = pending
            .values()
            .map(|p| p.until)
            .min()
            .unwrap_or_else(|| Instant::now() + WINDOW);
        let lost = tokio::select! {
            biased;
            () = tokio::time::sleep_until(until), if !pending.is_empty() => Some(LossReason::Deadline),
            reply = worker.reads.recv() => match reply.flatten() {
                Some(reply) => accept(&mut pending, reply, generation).err().map(|_| LossReason::Malformed),
                None => Some(LossReason::Ended),
            },
            command = commands.recv() => {
                let Some(command) = command else { return; };
                if command.answer.is_closed() { continue; }
                if pending.len() == QUEUE || command.until <= Instant::now() { complete::<K>(command.answer, Err(WorkerLost)); continue; }
                let Some(next) = id.checked_add(1) else { return; };
                id = next;
                pending.insert(id, Pending { until: command.until, answer: command.answer, assembly: Assembly::new(K::MAX_REPLY) });
                worker.writes.try_send((id, command.body)).err().map(|_| LossReason::Unwritable)
            }
        };
        let Some(reason) = lost else { continue };
        tracing::warn!(kind = K::NAME, ?reason, "worker lost");
        drop(worker);
        for (_, item) in std::mem::take(&mut pending) {
            complete::<K>(item.answer, Err(WorkerLost));
        }
        while let Ok(command) = commands.try_recv() {
            complete::<K>(command.answer, Err(WorkerLost));
        }
        if losses.send(generation).is_err() {
            tracing::debug!(kind = K::NAME, "worker lost without watchers");
        }
        // Every handle is gone, so a respawn would only land in a closing job.
        if commands.is_closed() {
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
                        Some(command) => complete::<K>(command.answer, Err(WorkerLost)),
                        None => return,
                    },
                }
            }
            let path = exe.clone();
            let job = Arc::clone(&jobs);
            match tokio::task::spawn_blocking(move || Worker::<K>::spawn(&path, &job)).await {
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
                complete::<K>(command.answer, Err(WorkerLost));
            }
        }
    }
}
