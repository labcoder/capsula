//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements. Once no run is active it also runs an
//! owed purge: a helper thread copies the store while this one keeps serving, this one swaps the
//! copy in, and a helper removes the purge's files. Page reads go to the page reader instead, and
//! checkpoints to the checkpoint helper ([`page_reader`]).

mod page_reader;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{Deleted, RunId, SceneId, ThreadId};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::coordinator::{Input, StoreOp};
use crate::store::{
    Cancel, Copied, Deletion, EventKind, NewEvent, Side, Store, StoreError, Swap, cleared_body,
};
use page_reader::{Barrier, Checkpoints, Exit, Read};

/// How often the thread looks at a running helper.
const POLL: Duration = Duration::from_millis(50);
/// Free space a copy leaves for foreground writes [guess]. shortcut: the snapshot's FTS rebuild
/// may grow it by about the index's size before old segments are freed, which this doesn't
/// count; add `events_fts_idx`'s size once indexes near 256 MiB.
const RESERVE: u64 = 256 * 1024 * 1024;
/// How long a ready swap keeps new reads and checkpoints from starting while it waits for the
/// ones running to end [guess]: a search on a large store takes about 350 ms.
const SWAP_WAIT: Duration = Duration::from_secs(2);

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
    /// Hears every op as it starts.
    #[cfg(test)]
    pub(crate) observed: Option<UnboundedSender<super::Seen>>,
    /// Gates that each hold one op before it runs.
    #[cfg(test)]
    pub(crate) gates: Option<mpsc::Receiver<Gate>>,
    /// Gates that each hold one page read with a snapshot open.
    #[cfg(test)]
    pub(crate) read_gates: Option<mpsc::Receiver<Gate>>,
    /// Holds the checkpoint helper after each checkpoint.
    #[cfg(test)]
    pub(crate) checkpoint_gate: Option<page_reader::HelperGate>,
    /// Where the first copy pauses.
    #[cfg(test)]
    pub(crate) pauses: Option<crate::store::Pauses>,
}

/// Holds the next op `matches` accepts, after it's reported started and before it runs, until
/// the test sends on `release`, so a test can take a lock or reorder pipes first.
#[cfg(test)]
pub(crate) struct Gate {
    pub(crate) matches: fn(&StoreOp) -> bool,
    pub(crate) release: mpsc::Receiver<()>,
}

#[cfg(test)]
impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").finish_non_exhaustive()
    }
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
            #[cfg(test)]
            observed: None,
            #[cfg(test)]
            gates: None,
            #[cfg(test)]
            read_gates: None,
            #[cfg(test)]
            checkpoint_gate: None,
            #[cfg(test)]
            pauses: None,
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
    /// A reader kept the old file open past `SWAP_WAIT`.
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
    /// A copy; any write stops it, and waits until it has let go of the live store.
    Copy {
        cancel: Arc<Cancel>,
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
    /// A clean copy waiting for the side connections, and since when.
    swap: Option<(Copied, Instant)>,
    side: Arc<Side>,
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
            swap: None,
            side: store.side(),
        }
    }

    /// How long the thread may wait for an op before the purge needs it.
    fn timeout(&self) -> Option<Duration> {
        if self.job.is_some() || self.swap.is_some() {
            return Some(POLL);
        }
        self.next
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// A write is about to run, so a running copy is stale. It stops, and the write waits until
    /// the copy holds no snapshot of the live store, which would keep the write's pages in the
    /// WAL for a later commit to checkpoint. The rest of the copy's work goes on off this thread.
    /// A swap waiting for the side connections is given up, and they start work again.
    fn op_arriving(&self) {
        if let Some(Job::Copy { cancel, .. }) = &self.job {
            cancel.stop_and_release();
        }
        if self.swap.is_some() {
            self.side.end_swap();
        }
    }

    /// An op ran: a copy that waited to swap is removed, and an owed purge waits for quiet
    /// again, but no sooner than a retry already set.
    fn op_arrived(&mut self, store: &Store) {
        if self.swap.take().is_some() {
            self.remove(store, Some(Attempt::Stale));
        }
        if self.job.is_none() && store.purge_pending() {
            let quiet = Instant::now() + self.schedule.quiet;
            self.next = Some(self.next.map_or(quiet, |next| next.max(quiet)));
        }
    }

    /// The wait for an op ran out: finishes a job that's done, or starts one that's due.
    fn tick(&mut self, store: &mut Store) {
        if let Some((copied, since)) = self.swap.take() {
            return self.try_swap(store, copied, since);
        }
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
                    self.side.begin_swap();
                    self.try_swap(store, copied, Instant::now());
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

    /// Swaps the copy in. While a side connection is in use it waits, polled, up to `SWAP_WAIT`
    /// since `since`, and none starts new work meanwhile.
    fn try_swap(&mut self, store: &mut Store, copied: Copied, since: Instant) {
        let attempt = match store.swap(&copied) {
            Ok(Swap::Blocked) if since.elapsed() < SWAP_WAIT => {
                #[cfg(test)]
                if let Some(observed) = &self.schedule.observed {
                    observed.send(super::Seen::SwapWaiting).ok();
                }
                self.swap = Some((copied, since));
                return;
            }
            Ok(Swap::Done) => Attempt::Done,
            Ok(Swap::Stale) => Attempt::Stale,
            Ok(Swap::Blocked) => Attempt::Blocked,
            Err(error) => failed(&error),
        };
        self.side.end_swap();
        // The old store, or the copy the swap didn't use, is left to remove.
        self.remove(store, Some(attempt));
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
        #[cfg(test)]
        let cancel = (self.schedule.pauses.take()).map_or_else(Cancel::default, Cancel::pausing);
        #[cfg(not(test))]
        let cancel = Cancel::default();
        let cancel = Arc::new(cancel);
        let cancelling = Arc::clone(&cancel);
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || owed.copy(&cancelling))
        {
            Ok(helper) => self.job = Some(Job::Copy { cancel, helper }),
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
    reads: mpsc::Sender<Read>,
    /// Ops other than reads queued so far; a read waits until that many have finished.
    accepted: AtomicU64,
    /// Fires when the thread has run every queued op and read and closed the store.
    done: oneshot::Receiver<()>,
}

impl StoreThread {
    pub(crate) fn spawn(
        mut store: Store,
        acks: UnboundedSender<Input>,
        #[cfg_attr(not(test), expect(unused_mut, reason = "tests take the read gates"))]
        mut schedule: Schedule,
    ) -> std::io::Result<Self> {
        let (ops_tx, ops) = mpsc::channel::<StoreOp>();
        let (reads_tx, reads) = mpsc::channel();
        let (finished, done) = oneshot::channel();
        // Made first, so a failed start below stops whatever already started.
        let exit = Exit {
            side: store.side(),
            barrier: Arc::new(Barrier::new(
                #[cfg(test)]
                schedule.observed.clone(),
            )),
            checkpoints: Arc::new(Checkpoints::default()),
        };
        let helper = page_reader::spawn_helper(
            &exit,
            #[cfg(test)]
            schedule.checkpoint_gate.take(),
        )?;
        let reader = page_reader::spawn_reader(
            reads,
            &exit,
            acks.clone(),
            #[cfg(test)]
            page_reader::Hooks {
                observed: schedule.observed.clone(),
                gates: schedule.read_gates.take(),
            },
        )?;
        thread::Builder::new()
            .name("store".to_owned())
            .spawn(move || {
                // Every queued op runs, even with nobody left to hear the acknowledgement.
                // shortcut: in a debug build a panic here leaves queued ops unanswered, so their runs
                // never leave the coordinator and new turns stop once 16 pile up; release builds
                // abort on panic instead. Restart the brain on a store panic once it has a supervisor.
                let mut purger = Purger::new(schedule, &store);
                #[cfg(test)]
                let mut gates: Vec<Gate> = Vec::new();
                loop {
                    let op = match purger.timeout() {
                        None => ops.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(timeout) => ops.recv_timeout(timeout),
                    };
                    match op {
                        Ok(op) => {
                            #[cfg(test)]
                            {
                                if let Some(observed) = &purger.schedule.observed {
                                    observed.send(super::Seen::StoreOpStarted(op.clone())).ok();
                                }
                                if let Some(added) = &purger.schedule.gates {
                                    gates.extend(added.try_iter());
                                }
                                if let Some(at) = gates.iter().position(|gate| (gate.matches)(&op))
                                {
                                    gates.remove(at).release.recv().ok();
                                }
                            }
                            serve_op(&mut store, &mut purger, &acks, op);
                            exit.barrier.complete();
                            exit.checkpoints.raise();
                            #[cfg(test)]
                            if let Some(observed) = &purger.schedule.observed {
                                observed.send(super::Seen::CheckpointOwed).ok();
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => purger.tick(&mut store),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let job = purger.stop();
                // Every queued read's ops have run, so the reader finishes them and ends.
                if reader.join().is_err() {
                    tracing::error!("the page reader panicked");
                }
                drop(exit);
                if helper.join().is_err() {
                    tracing::error!("the checkpoint helper panicked");
                }
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
        Ok(Self {
            ops: ops_tx,
            reads: reads_tx,
            accepted: AtomicU64::new(0),
            done,
        })
    }

    /// Queues an operation, a page read with the page reader. False when the thread has gone,
    /// which only a panic there causes.
    pub(crate) fn send(&self, op: StoreOp) -> bool {
        if let StoreOp::ReadPage {
            epoch,
            request,
            page,
        } = op
        {
            let stamp = self.accepted.load(Ordering::Relaxed);
            return (self.reads)
                .send(Read {
                    stamp,
                    epoch,
                    request,
                    page,
                })
                .is_ok();
        }
        let sent = self.ops.send(op).is_ok();
        if sent {
            self.accepted.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    /// Lets the queued operations and reads finish, then closes the store; a running copy stops,
    /// and the purge stays owed. The receiver fires when it has, and fails if the thread panicked;
    /// nothing blocks on the thread itself.
    pub(crate) fn close(self) -> oneshot::Receiver<()> {
        drop(self.ops);
        self.done
    }
}

#[expect(clippy::too_many_lines, reason = "one short arm per store op")]
fn serve_op(store: &mut Store, purger: &mut Purger, acks: &UnboundedSender<Input>, op: StoreOp) {
    match op {
        StoreOp::Drawing { serial, write } => {
            purger.op_arriving();
            let reply = store
                .reconnect()
                .and_then(|()| store.drawing_write(&write))
                .map_err(|_| ());
            if reply.is_err() {
                tracing::warn!("a drawing store operation failed");
            }
            answer(acks, Input::DrawingWritten { serial, reply });
            purger.op_arrived(store);
        }
        StoreOp::ReadPage { .. } => unreachable!("StoreThread::send hands reads to the reader"),
        StoreOp::ClearScenes { scenes } => {
            purger.op_arriving();
            answer(acks, clear(store, scenes));
            purger.op_arrived(store);
        }
        StoreOp::KeepScene { scene, revealed } => {
            purger.op_arriving();
            let ok = match store
                .reconnect()
                .and_then(|()| store.keep_scene(scene, &revealed))
            {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, "a kept selection failed; it's retried");
                    false
                }
            };
            answer(acks, Input::SceneKept { scene, ok });
            purger.op_arrived(store);
        }
        StoreOp::Delete { thread } => {
            purger.op_arriving();
            answer(acks, delete(store, thread));
            purger.op_arrived(store);
        }
        StoreOp::SetTitle {
            thread,
            title,
            source,
        } => unanswered(store, purger, thread, "title", |store| {
            store.set_title(thread, &title, source)
        }),
        StoreOp::SetModel { thread, choice } => {
            unanswered(store, purger, thread, "model choice", |store| {
                store.set_model(thread, &choice)
            });
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

/// Deletes a thread; an unavailable store hides nothing. A failure is logged by kind only.
fn delete(store: &mut Store, thread: ThreadId) -> Input {
    let deletion = match store.reconnect() {
        Ok(()) => store.delete(thread),
        Err(error) => Deletion {
            outcome: Deleted::NotHidden,
            last_run: None,
            error: Some(error),
        },
    };
    if let Some(error) = &deletion.error {
        tracing::warn!(%error, outcome = ?deletion.outcome, "a delete didn't finish");
    }
    Input::Deleted {
        thread,
        outcome: deletion.outcome,
        last_run: deletion.last_run,
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

/// Runs a write nothing waits on, holding off the purge meanwhile. A failure is logged by kind
/// and thread id only.
fn unanswered(
    store: &mut Store,
    purger: &mut Purger,
    thread: ThreadId,
    what: &'static str,
    op: impl FnOnce(&mut Store) -> Result<(), StoreError>,
) {
    purger.op_arriving();
    if let Err(error) = store.reconnect().and_then(|()| op(store)) {
        tracing::warn!(%error, thread = thread.0, what, "a thread change wasn't stored");
    }
    purger.op_arrived(store);
}

fn answer(acks: &UnboundedSender<Input>, reply: Input) {
    if acks.send(reply).is_err() {
        tracing::debug!("an acknowledgement had no listener");
    }
}

#[cfg(test)]
mod tests;
