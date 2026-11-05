use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, mpsc as blocking};
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use kumochi_protocol::geometry::DisplayRef;

use super::wire::{
    Assembly, Exclusion, FrameMeta, Refusal, Reply, ReplyBody, Request, RequestBody, ScreenError,
    TicketWire,
};
use crate::platform::{Helper, Job};

/// Absolute request budget, including queueing (guess).
pub const REQUEST_DEADLINE: Duration = Duration::from_millis(1500);
const WINDOW: Duration = Duration::from_secs(60);
const QUEUE: usize = 16;
type Answer = oneshot::Sender<Result<ReplyBody, ScreenError>>;

/// Why a capture produced no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The worker refused the ticket or the display.
    #[error("capture refused: {0:?}")]
    Refused(Refusal),
    /// The worker failed or was lost.
    #[error(transparent)]
    Screen(#[from] ScreenError),
}

/// Every watch owned by the previous worker is now invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerLost;

/// The isolated worker's lifecycle owner.
#[derive(Debug)]
pub struct ScreenSupervisor;

/// Requests and loss notifications; dropping the last handle stops the worker.
#[derive(Clone, Debug)]
pub struct ScreenHandle {
    commands: mpsc::Sender<Command>,
    losses: broadcast::Sender<WorkerLost>,
    deadline: Duration,
}

struct Command {
    body: RequestBody,
    until: Instant,
    answer: Answer,
}

impl ScreenHandle {
    /// Submits one operation. Backoff and a full queue fail fast.
    ///
    /// # Errors
    /// Returns `WorkerLost` on a deadline, transport failure, overload or shutdown.
    pub async fn request(&self, body: RequestBody) -> Result<ReplyBody, ScreenError> {
        let until = Instant::now() + self.deadline;
        let (answer, reply) = oneshot::channel();
        self.commands
            .try_send(Command {
                body,
                until,
                answer,
            })
            .map_err(|_| ScreenError::WorkerLost)?;
        tokio::time::timeout_at(until, reply)
            .await
            .map_err(|_| ScreenError::WorkerLost)?
            .unwrap_or(Err(ScreenError::WorkerLost))
    }

    /// Takes one frame under `ticket`; the pixels stay in the worker. The worker's validity runs
    /// from its receipt, so the caller (B2a) still discards a reply past the ticket's own deadline.
    ///
    /// # Errors
    /// The worker's refusal, a platform fault, or `WorkerLost` for any reply that isn't this
    /// capture's.
    pub async fn capture(
        &self,
        ticket: TicketWire,
        display: DisplayRef,
        exclude: Exclusion,
    ) -> Result<FrameMeta, CaptureError> {
        let capture = ticket.capture();
        let body = RequestBody::Capture {
            ticket,
            display,
            exclude,
        };
        match self.request(body).await? {
            ReplyBody::Captured(meta) if meta.capture == capture && meta.display == display.0 => {
                Ok(meta)
            }
            ReplyBody::Refused(refusal) => Err(CaptureError::Refused(refusal)),
            _ => Err(ScreenError::WorkerLost.into()),
        }
    }

    /// Subscribes before issuing work, so a lost watch generation cannot be missed.
    pub fn lost(&self) -> broadcast::Receiver<WorkerLost> {
        self.losses.subscribe()
    }
}

impl ScreenSupervisor {
    /// Starts the worker directly in the brain job. macOS uses `posix_spawn` file actions
    /// and a process-group kill behind the same platform boundary.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn start(exe: PathBuf, jobs: Arc<Job>) -> io::Result<ScreenHandle> {
        Self::with_deadline(exe, jobs, REQUEST_DEADLINE)
    }

    /// Starts with an injected budget so fixtures do not race a product deadline.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn with_deadline(
        exe: PathBuf,
        jobs: Arc<Job>,
        deadline: Duration,
    ) -> io::Result<ScreenHandle> {
        let worker = Worker::spawn(&exe, &jobs)?;
        let (commands, receive) = mpsc::channel(QUEUE);
        let (losses, _) = broadcast::channel(QUEUE);
        let task = tokio::spawn(run(exe, jobs, worker, receive, losses.clone()));
        drop(task);
        Ok(ScreenHandle {
            commands,
            losses,
            deadline,
        })
    }
}

struct Worker {
    process: Helper,
    writes: blocking::SyncSender<Request>,
    reads: mpsc::Receiver<Option<Reply>>,
}

impl Worker {
    fn spawn(exe: &std::path::Path, job: &Job) -> io::Result<Self> {
        let (process, mut pipes) = Job::spawn_piped(exe, crate::SCREEN_WORKER, &[job])?;
        let (writes, requests) = blocking::sync_channel::<Request>(QUEUE);
        let (replies, reads) = mpsc::channel(QUEUE);
        let worker = Self {
            process,
            writes,
            reads,
        };
        let errors = replies.clone();
        let reader = std::thread::Builder::new()
            .name("screen-reader".into())
            .spawn(move || {
                loop {
                    let reply = frame::read(&mut pipes.stdout, Pipe::Data).ok().flatten();
                    let ended = reply.is_none();
                    if replies.blocking_send(reply).is_err() || ended {
                        break;
                    }
                }
            })?;
        drop(reader);
        let outbound = std::thread::Builder::new()
            .name("screen-writer".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    if frame::write(&mut pipes.stdin, &request, Pipe::Data).is_err() {
                        if errors.blocking_send(None).is_err() {
                            return;
                        }
                        break;
                    }
                }
            })?;
        drop(outbound);
        Ok(worker)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Err(error) = self.process.terminate() {
            tracing::error!(code = error.raw_os_error(), "screen termination failed");
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
    /// The worker's stdout ended or failed.
    Ended,
    /// A request couldn't reach the writer thread.
    Unwritable,
}

struct Pending {
    until: Instant,
    answer: Answer,
    assembly: Assembly,
}

fn complete(answer: Answer, reply: Result<ReplyBody, ScreenError>) {
    if answer.send(reply).is_err() {
        tracing::debug!("screen requester cancelled");
    }
}

fn accept(pending: &mut BTreeMap<u64, Pending>, reply: Reply) -> Result<(), ScreenError> {
    let id = reply.id;
    let item = pending.get_mut(&id).ok_or(ScreenError::WorkerLost)?;
    if let Some(bytes) = item.assembly.push(reply)? {
        let body = frame::decode::<ReplyBody>(&bytes).map_err(|_| ScreenError::WorkerLost)?;
        let item = pending.remove(&id).ok_or(ScreenError::WorkerLost)?;
        complete(
            item.answer,
            match body {
                ReplyBody::Fault(code) => Err(ScreenError::Fault(code)),
                body => Ok(body),
            },
        );
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

async fn run(
    exe: PathBuf,
    jobs: Arc<Job>,
    mut worker: Worker,
    mut commands: mpsc::Receiver<Command>,
    losses: broadcast::Sender<WorkerLost>,
) {
    let mut history = VecDeque::new();
    let mut pending: BTreeMap<u64, Pending> = BTreeMap::new();
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
                Some(reply) => accept(&mut pending, reply).err().map(|_| LossReason::Malformed),
                None => Some(LossReason::Ended),
            },
            command = commands.recv() => {
                let Some(command) = command else { return; };
                if command.answer.is_closed() { continue; }
                if pending.len() == QUEUE || command.until <= Instant::now() { complete(command.answer, Err(ScreenError::WorkerLost)); continue; }
                let Some(next) = id.checked_add(1) else { return; };
                id = next;
                pending.insert(id, Pending { until: command.until, answer: command.answer, assembly: Assembly::default() });
                worker.writes.try_send(Request { id, body: command.body }).err().map(|_| LossReason::Unwritable)
            }
        };
        let Some(reason) = lost else { continue };
        tracing::warn!(?reason, "screen worker lost");
        drop(worker);
        for (_, item) in std::mem::take(&mut pending) {
            complete(item.answer, Err(ScreenError::WorkerLost));
        }
        while let Ok(command) = commands.try_recv() {
            complete(command.answer, Err(ScreenError::WorkerLost));
        }
        if losses.send(WorkerLost).is_err() {
            tracing::debug!("screen lost without watchers");
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
                        Some(command) => complete(command.answer, Err(ScreenError::WorkerLost)),
                        None => return,
                    },
                }
            }
            let path = exe.clone();
            let job = Arc::clone(&jobs);
            match tokio::task::spawn_blocking(move || Worker::spawn(&path, &job)).await {
                Ok(Ok(next)) => {
                    tracing::warn!(delay_ms = delay.as_millis(), "screen worker respawned");
                    worker = next;
                    break;
                }
                Ok(Err(error)) => {
                    tracing::error!(code = error.raw_os_error(), "screen spawn failed");
                }
                Err(_) => tracing::error!(code = 2, "screen spawn task failed"),
            }
            while let Ok(command) = commands.try_recv() {
                complete(command.answer, Err(ScreenError::WorkerLost));
            }
        }
    }
}

#[cfg(test)]
mod tests;
