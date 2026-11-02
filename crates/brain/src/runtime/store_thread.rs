//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements. Once no run is active it also runs an
//! owed purge: a helper thread copies the store while this one keeps serving, this one swaps the
//! copy in, and a helper removes the purge's files.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use capsula_protocol::message::{DataToShell, PageOf, RunId, SceneId};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::coordinator::{Input, StoreOp};
use crate::store::{Copied, EventKind, NewEvent, Store, StoreError, Swap, cleared_body};

/// How often the thread looks at a running helper.
const POLL: Duration = Duration::from_millis(50);
/// Free space a copy leaves for foreground writes [guess]. shortcut: the snapshot's FTS rebuild
/// may grow it by about the index's size before old segments are freed, which this doesn't
/// count; add `events_fts_idx`'s size once indexes near 256 MiB.
const RESERVE: u64 = 256 * 1024 * 1024;

/// When an owed purge is tried.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// How long after the last op [guess].
    pub(crate) quiet: Duration,
    /// The wait after an attempt that didn't finish, doubling up to `last_retry`, so a store
    /// that keeps being written, a full disk or a stuck file isn't rewritten constantly [guess].
    pub(crate) first_retry: Duration,
    pub(crate) last_retry: Duration,
    /// The volume's free space for a folder.
    pub(crate) free_space: fn(&Path) -> io::Result<u64>,
    /// Hears every attempt's outcome.
    #[cfg(test)]
    pub(crate) attempts: Option<mpsc::Sender<Attempt>>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(200),
            first_retry: Duration::from_secs(60),
            last_retry: Duration::from_mins(30),
            free_space: crate::platform::free_space,
            #[cfg(test)]
            attempts: None,
        }
    }
}

/// What a purge attempt came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// A run was active, so it waits for the next op.
    Busy,
    /// An op arrived during the copy, or after it, so the copy was thrown away.
    Stale,
    /// A reader kept the old file open.
    Blocked,
    /// The volume hasn't room for the copies and the foreground's reserve.
    NoRoom,
    /// SQLite or the file system failed.
    Failed,
    /// The clean copy replaced the store and every purge file is gone.
    Done,
}

/// The one purge job running on a helper thread. Only one runs at a time, so a removal never
/// takes a later copy's file.
enum Job {
    /// A copy; any op stops it.
    Copy {
        stop: Arc<AtomicBool>,
        helper: JoinHandle<Result<Option<Copied>, StoreError>>,
    },
    /// Removing the purge's files: the old store after a swap, an unused copy, or what a crash
    /// left. Ops don't stop it, and it runs during runs too: until it's done, the old store's
    /// deleted text is still on disk.
    Remove {
        helper: JoinHandle<io::Result<()>>,
        /// What the attempt came to once the files are gone; `None` goes on to an owed copy.
        then: Option<Attempt>,
    },
}

impl Job {
    fn is_finished(&self) -> bool {
        match self {
            Self::Copy { helper, .. } => helper.is_finished(),
            Self::Remove { helper, .. } => helper.is_finished(),
        }
    }

    /// Waits for the helper; false when it panicked.
    fn join(self) -> bool {
        match self {
            Self::Copy { helper, .. } => helper.join().is_ok(),
            Self::Remove { helper, .. } => helper.join().is_ok(),
        }
    }
}

/// The purge's side of the thread: what's running and when to try next.
struct Purger {
    schedule: Schedule,
    next: Option<Instant>,
    retry: Duration,
    job: Option<Job>,
}

impl Purger {
    /// Due at once when a purge is owed or its files are left from an earlier session.
    fn new(schedule: Schedule, store: &Store) -> Self {
        let due = store.purge_pending() || store.leftovers().exist();
        Self {
            retry: schedule.first_retry,
            next: due.then(Instant::now),
            schedule,
            job: None,
        }
    }

    /// How long the thread may wait for an op before the purge needs it.
    fn timeout(&self) -> Option<Duration> {
        if self.job.is_some() {
            return Some(POLL);
        }
        self.next
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// An op is about to run, so a running copy is stale: it stops before the op takes the disk.
    fn op_arriving(&self) {
        if let Some(Job::Copy { stop, .. }) = &self.job {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// An op ran: an owed purge waits for quiet again, but no sooner than a retry already set.
    fn op_arrived(&mut self, store: &Store) {
        if self.job.is_none() && store.purge_pending() {
            let quiet = Instant::now() + self.schedule.quiet;
            self.next = Some(self.next.map_or(quiet, |next| next.max(quiet)));
        }
    }

    /// The wait for an op ran out: finishes a job that's done, or starts one that's due.
    fn tick(&mut self, store: &mut Store) {
        let Some(job) = self.job.take_if(|job| job.is_finished()) else {
            if self.job.is_none() && self.next.is_some_and(|at| at <= Instant::now()) {
                self.start(store);
            }
            return;
        };
        self.finish(store, job);
    }

    fn finish(&mut self, store: &mut Store, job: Job) {
        match job {
            Job::Copy { helper, .. } => match helper.join() {
                Ok(Ok(Some(copied))) => {
                    let attempt = match store.swap(&copied) {
                        Ok(Swap::Done) => Attempt::Done,
                        Ok(Swap::Stale) => Attempt::Stale,
                        Ok(Swap::Blocked) => Attempt::Blocked,
                        Err(error) => failed(&error),
                    };
                    // The old store, or the copy the swap didn't use, is left to remove.
                    self.remove(store, Some(attempt));
                }
                Ok(Ok(None)) => self.settle(Attempt::Stale, store),
                Ok(Err(error)) => self.settle(failed(&error), store),
                Err(_) => {
                    tracing::error!("the purge's helper panicked");
                    self.settle(Attempt::Failed, store);
                }
            },
            Job::Remove { helper, then } => match (helper.join(), then) {
                (Ok(Ok(())), Some(attempt)) => self.settle(attempt, store),
                (Ok(Ok(())), None) if store.purge_pending() => self.next = Some(Instant::now()),
                (Ok(Ok(())), None) => self.settle(Attempt::Done, store),
                (Ok(Err(error)), _) => {
                    tracing::warn!(%error, "a purge file couldn't be removed; it's retried later");
                    self.settle(Attempt::Failed, store);
                }
                (Err(_), _) => {
                    tracing::error!("the purge's helper panicked");
                    self.settle(Attempt::Failed, store);
                }
            },
        }
    }

    /// Removes what an earlier attempt left, or else copies the store if a purge is owed, no
    /// run is active and the volume has room.
    fn start(&mut self, store: &mut Store) {
        if let Err(error) = store.reconnect() {
            return self.settle(failed(&error), store);
        }
        if store.leftovers().exist() {
            return self.remove(store, None);
        }
        let owed = match store.owed() {
            Ok(Some(owed)) => owed,
            // The swap went through but a failed reopen kept it from settling.
            Ok(None) if !store.purge_pending() => return self.settle(Attempt::Done, store),
            Ok(None) => return self.settle(Attempt::Busy, store),
            Err(error) => return self.settle(failed(&error), store),
        };
        match (self.schedule.free_space)(owed.folder()) {
            Ok(free) if free >= owed.needs().saturating_add(RESERVE) => {}
            Ok(_) => return self.settle(Attempt::NoRoom, store),
            Err(error) => return self.settle(failed(&StoreError::Purge(error)), store),
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || owed.copy(&stopping))
        {
            Ok(helper) => self.job = Some(Job::Copy { stop, helper }),
            Err(error) => {
                tracing::error!(%error, "the purge's helper didn't start");
                self.settle(Attempt::Failed, store);
            }
        }
    }

    fn remove(&mut self, store: &Store, then: Option<Attempt>) {
        let leftovers = store.leftovers();
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || leftovers.remove())
        {
            Ok(helper) => self.job = Some(Job::Remove { helper, then }),
            Err(error) => {
                tracing::error!(%error, "the purge's helper didn't start");
                self.settle(Attempt::Failed, store);
            }
        }
    }

    fn settle(&mut self, attempt: Attempt, store: &Store) {
        let now = Instant::now();
        self.next = match attempt {
            // A deletion while the files were removed owes another purge.
            Attempt::Done => store.purge_pending().then(|| now + self.schedule.quiet),
            Attempt::Busy => None,
            Attempt::Stale | Attempt::Blocked | Attempt::NoRoom | Attempt::Failed => {
                let at = now + self.retry;
                self.retry = (self.retry * 2).min(self.schedule.last_retry);
                Some(at)
            }
        };
        if attempt == Attempt::Done {
            self.retry = self.schedule.first_retry;
        }
        #[cfg(test)]
        if let Some(attempts) = &self.schedule.attempts {
            attempts.send(attempt).ok();
        }
    }

    /// Tells a running copy to stop, and hands back the job to wait for.
    fn stop(self) -> Option<Job> {
        self.op_arriving();
        self.job
    }
}

fn failed(error: &StoreError) -> Attempt {
    tracing::error!(%error, "the purge failed; it's retried later");
    Attempt::Failed
}

#[derive(Debug)]
pub(crate) struct StoreThread {
    ops: mpsc::Sender<StoreOp>,
    /// Fires when the thread has run every queued op and closed the store.
    done: oneshot::Receiver<()>,
}

impl StoreThread {
    pub(crate) fn spawn(
        mut store: Store,
        acks: UnboundedSender<Input>,
        schedule: Schedule,
    ) -> std::io::Result<Self> {
        let (ops_tx, ops) = mpsc::channel();
        let (finished, done) = oneshot::channel();
        thread::Builder::new()
            .name("store".to_owned())
            .spawn(move || {
                // Every queued op runs, even with nobody left to hear the acknowledgement.
                // shortcut: in a debug build a panic here leaves queued ops unanswered, so their runs
                // never leave the coordinator and new turns stop once 16 pile up; release builds
                // abort on panic instead. Restart the brain on a store panic once it has a supervisor.
                let mut purger = Purger::new(schedule, &store);
                loop {
                    let op = match purger.timeout() {
                        None => ops.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(timeout) => ops.recv_timeout(timeout),
                    };
                    match op {
                        Ok(op) => {
                            serve_op(&mut store, &mut purger, &acks, op);
                        }
                        Err(RecvTimeoutError::Timeout) => purger.tick(&mut store),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let job = purger.stop();
                drop(store);
                if finished.send(()).is_err() {
                    tracing::debug!("nobody waited for the store to close");
                }
                // After the store closed: a copy's sync and a removal can't be stopped, and close
                // needn't wait for them. Files the exit cuts short are removed at the next start.
                if job.is_some_and(|job| !job.join()) {
                    tracing::error!("the purge's helper panicked");
                }
            })?;
        Ok(Self { ops: ops_tx, done })
    }

    /// Queues an operation. False when the thread has gone, which only a panic there causes.
    pub(crate) fn send(&self, op: StoreOp) -> bool {
        self.ops.send(op).is_ok()
    }

    /// Lets the queued operations finish, then closes the store; a running copy stops, and the
    /// purge stays owed. The receiver fires when it has, and fails if the thread panicked;
    /// nothing blocks on the thread itself.
    pub(crate) fn close(self) -> oneshot::Receiver<()> {
        drop(self.ops);
        self.done
    }
}

fn serve_op(store: &mut Store, purger: &mut Purger, acks: &UnboundedSender<Input>, op: StoreOp) {
    match op {
        StoreOp::ReadPage {
            epoch,
            request,
            page,
        } => {
            answer(acks, read(store, epoch, request, page));
            purger.tick(store);
        }
        StoreOp::ClearScenes { scenes } => {
            purger.op_arriving();
            answer(acks, clear(store, scenes));
            purger.op_arrived(store);
        }
        StoreOp::BeginTurn {
            thread,
            title,
            run,
            text,
        } => write(store, purger, acks, run, "turn", |store| {
            store
                .begin_turn(thread, title.as_deref(), run, &text)
                .map(|()| Input::TurnStored { run })
        }),
        StoreOp::CommitStep {
            thread,
            run,
            text,
            scene,
            index,
            step,
        } => write(store, purger, acks, run, "step", |store| {
            store
                .commit_step((thread, run), text.as_deref(), scene, index, &step.label)
                .map(|scene| Input::StepStored {
                    run,
                    index,
                    step,
                    scene,
                })
        }),
        StoreOp::EndRun {
            thread,
            run,
            text,
            revealed,
            clear,
            end,
        } => write(store, purger, acks, run, "outcome", |store| {
            let revealed = revealed.map(|steps| serde_json::json!(steps).to_string());
            let clear = clear.map(cleared_body);
            let mut events = Vec::with_capacity(3);
            if let Some(text) = &text {
                events.push(NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some(text.as_str()),
                    body: None,
                });
            }
            if let Some(revealed) = &revealed {
                events.push(NewEvent {
                    kind: EventKind::Revealed,
                    text: None,
                    body: Some(revealed.as_str()),
                });
            }
            if let Some(clear) = &clear {
                events.push(NewEvent {
                    kind: EventKind::SceneCleared,
                    text: None,
                    body: Some(clear.as_str()),
                });
            }
            store
                .end_run(thread, run, &events, end)
                .map(|()| Input::RunStored { run })
        }),
    }
}

/// Reads one page, never holding off the purge. A failure is logged by kind only.
fn read(store: &mut Store, epoch: u64, request: u32, page: PageOf) -> Input {
    let reply = store
        .reconnect()
        .and_then(|()| store.read_page(request, page));
    if let Err(error) = &reply {
        tracing::warn!(%error, "a page read failed");
    }
    Input::PageRead {
        epoch,
        request,
        reply: reply.unwrap_or(DataToShell::PageFailed { request }),
    }
}

/// Writes a clear. A failed one is retried, so it's only logged, by how many scenes it names.
fn clear(store: &mut Store, scenes: Vec<SceneId>) -> Input {
    let ok = match store.reconnect().and_then(|()| store.clear_scenes(&scenes)) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, scenes = scenes.len(), "a clear failed; it's retried");
            false
        }
    };
    Input::ScenesCleared { scenes, ok }
}

/// Runs one write in one transaction and answers it, holding off the purge meanwhile. Errors are
/// logged by kind and run id only, never content.
fn write(
    store: &mut Store,
    purger: &mut Purger,
    acks: &UnboundedSender<Input>,
    run: RunId,
    what: &'static str,
    op: impl FnOnce(&mut Store) -> Result<Input, StoreError>,
) {
    purger.op_arriving();
    // After a swap's reopen failed: fails closed until the file opens again.
    let reply = match store.reconnect().map(|()| op(store)) {
        Err(error) => {
            tracing::warn!(%error, run = run.0, "the store is unavailable");
            Input::StoreFailed { run }
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, run = run.0, what, "a store operation failed");
            Input::StoreFailed { run }
        }
        Ok(Ok(stored)) => stored,
    };
    answer(acks, reply);
    purger.op_arrived(store);
}

fn answer(acks: &UnboundedSender<Input>, reply: Input) {
    if acks.send(reply).is_err() {
        tracing::debug!("an acknowledgement had no listener");
    }
}

#[cfg(test)]
mod tests;
