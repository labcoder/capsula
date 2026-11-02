//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements. Once no run is active it also runs an
//! owed purge: a helper thread copies the store while this one keeps serving, then this one swaps
//! the copy in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::coordinator::{Input, StoreOp};
use crate::store::{Copied, EventKind, NewEvent, Store, StoreError, Swap};

/// How often the thread looks at a running copy.
const POLL: Duration = Duration::from_millis(50);

/// When an owed purge is tried.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// How long after the last op [guess].
    pub(crate) quiet: Duration,
    /// The wait after an attempt that failed or met a reader, doubling up to `last_retry`, so a
    /// full disk isn't rewritten constantly [guess].
    pub(crate) first_retry: Duration,
    pub(crate) last_retry: Duration,
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
    /// SQLite or the file system failed.
    Failed,
    /// The clean copy replaced the store.
    Done,
}

/// A copy running on its helper thread.
struct Copying {
    stop: Arc<AtomicBool>,
    helper: JoinHandle<Result<Option<Copied>, StoreError>>,
}

/// The purge's side of the thread: what's running and when to try next.
struct Purger {
    schedule: Schedule,
    next: Option<Instant>,
    retry: Duration,
    copying: Option<Copying>,
}

impl Purger {
    /// How long the thread may wait for an op before the purge needs it.
    fn timeout(&self) -> Option<Duration> {
        if self.copying.is_some() {
            return Some(POLL);
        }
        self.next
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// An op arrived: a running copy is stale, and an owed purge waits for quiet again, but no
    /// sooner than a retry already set.
    fn op_arrived(&mut self, store: &Store) {
        if let Some(copying) = &self.copying {
            copying.stop.store(true, Ordering::Relaxed);
        } else if store.purge_pending() {
            let quiet = Instant::now() + self.schedule.quiet;
            self.next = Some(self.next.map_or(quiet, |next| next.max(quiet)));
        }
    }

    /// The wait for an op ran out: finishes a copy that's done, or starts one that's due.
    fn tick(&mut self, store: &mut Store) {
        if let Some(copying) = self.copying.take_if(|copying| copying.helper.is_finished()) {
            let attempt = match copying.helper.join() {
                Ok(Ok(Some(copied))) => match store.swap(copied) {
                    Ok(Swap::Done) => Attempt::Done,
                    Ok(Swap::Stale) => Attempt::Stale,
                    Ok(Swap::Blocked) => Attempt::Blocked,
                    Err(error) => failed(&error),
                },
                Ok(Ok(None)) => Attempt::Stale,
                Ok(Err(error)) => failed(&error),
                Err(_) => {
                    tracing::error!("the purge's helper panicked");
                    Attempt::Failed
                }
            };
            self.settle(attempt);
        } else if self.copying.is_none() && self.next.is_some_and(|at| at <= Instant::now()) {
            self.start(store);
        }
    }

    fn start(&mut self, store: &Store) {
        let owed = match store.owed() {
            Ok(Some(owed)) => owed,
            Ok(None) => return self.settle(Attempt::Busy),
            Err(error) => return self.settle(failed(&error)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || owed.copy(&stopping))
        {
            Ok(helper) => self.copying = Some(Copying { stop, helper }),
            Err(error) => {
                tracing::error!(%error, "the purge's helper didn't start");
                self.settle(Attempt::Failed);
            }
        }
    }

    fn settle(&mut self, attempt: Attempt) {
        let now = Instant::now();
        self.next = match attempt {
            Attempt::Done | Attempt::Busy => None,
            Attempt::Stale => Some(now + self.schedule.quiet),
            Attempt::Blocked | Attempt::Failed => {
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

    /// Tells a running copy to stop, and hands back its helper to wait for.
    fn stop(self) -> Option<JoinHandle<Result<Option<Copied>, StoreError>>> {
        let copying = self.copying?;
        copying.stop.store(true, Ordering::Relaxed);
        Some(copying.helper)
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
                let mut purger = Purger {
                    retry: schedule.first_retry,
                    next: store.purge_pending().then(Instant::now),
                    schedule,
                    copying: None,
                };
                loop {
                    let op = match purger.timeout() {
                        None => ops.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(timeout) => ops.recv_timeout(timeout),
                    };
                    match op {
                        Ok(op) => {
                            if acks.send(apply(&mut store, op)).is_err() {
                                tracing::debug!("an acknowledgement had no listener");
                            }
                            purger.op_arrived(&store);
                        }
                        Err(RecvTimeoutError::Timeout) => purger.tick(&mut store),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let helper = purger.stop();
                drop(store);
                if finished.send(()).is_err() {
                    tracing::debug!("nobody waited for the store to close");
                }
                // After the store closed: a copy's sync can't be stopped, and close needn't wait
                // for it. A copy the exit cuts short is removed at the next start.
                if helper.is_some_and(|helper| helper.join().is_err()) {
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

/// Runs one operation in one transaction and answers it. Errors are logged by kind and run id
/// only, never content.
fn apply(store: &mut Store, op: StoreOp) -> Input {
    let run = op.run();
    let (result, stored, what) = match op {
        StoreOp::BeginTurn {
            thread,
            title,
            run,
            text,
        } => (
            store.begin_turn(thread, title.as_deref(), run, &text),
            Input::TurnStored { run },
            "turn",
        ),
        StoreOp::CommitStep {
            thread,
            run,
            text,
            index,
            step,
        } => {
            let body = serde_json::json!({ "index": index, "label": step.label }).to_string();
            let mut events = Vec::with_capacity(2);
            if let Some(text) = &text {
                events.push(NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some(text.as_str()),
                    body: None,
                });
            }
            events.push(NewEvent {
                kind: EventKind::SceneStep,
                text: Some(step.label.as_str()),
                body: Some(body.as_str()),
            });
            (
                store.append(thread, run, &events),
                Input::StepStored { run, index, step },
                "step",
            )
        }
        StoreOp::EndRun {
            thread,
            run,
            text,
            revealed,
            end,
        } => {
            let revealed = revealed.map(|steps| serde_json::json!(steps).to_string());
            let mut events = Vec::with_capacity(2);
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
            (
                store.end_run(thread, run, &events, end),
                Input::RunStored { run },
                "outcome",
            )
        }
    };
    match result {
        Ok(()) => stored,
        Err(error) => {
            tracing::warn!(%error, run = run.0, what, "a store operation failed");
            Input::StoreFailed { run }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use capsula_protocol::message::{RunId, Step, ThreadId};
    use rusqlite::Connection;
    use tokio::sync::mpsc;

    use super::{Attempt, Schedule, StoreThread};
    use crate::coordinator::{Input, StoreOp};
    use crate::store::{RunEnd, Store};
    use crate::test_support::Scratch;

    /// A store owing the purge of its deleted thread 1.
    fn owing(scratch: &Scratch) -> Store {
        let mut store = Store::open(&scratch.db()).expect("store");
        store
            .begin_turn(ThreadId(1), Some("t"), RunId(1), "doomed")
            .expect("turn");
        store.begin_delete(ThreadId(1)).expect("begin");
        store.finish_delete(ThreadId(1)).expect("finish");
        store
    }

    /// The store's thread with short waits, and every purge attempt it makes.
    fn spawn(store: Store) -> (StoreThread, std_mpsc::Receiver<Attempt>) {
        let (acks, _replies) = mpsc::unbounded_channel();
        let (heard, attempts) = std_mpsc::channel();
        let schedule = Schedule {
            quiet: Duration::from_millis(1),
            first_retry: Duration::from_millis(20),
            last_retry: Duration::from_millis(20),
            attempts: Some(heard),
        };
        let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
        (thread, attempts)
    }

    fn next(attempts: &std_mpsc::Receiver<Attempt>) -> Attempt {
        attempts
            .recv_timeout(Duration::from_secs(10))
            .expect("the store's thread tried the purge")
    }

    /// Waits out repeats a retry already under way can add, then expects the purge to finish.
    fn done_after(attempts: &std_mpsc::Receiver<Attempt>, repeats: &[Attempt]) {
        let mut next_one = next(attempts);
        while repeats.contains(&next_one) {
            next_one = next(attempts);
        }
        assert_eq!(next_one, Attempt::Done);
    }

    fn owed(scratch: &Scratch) -> i64 {
        Connection::open(scratch.db())
            .expect("watcher")
            .query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn an_owed_purge_waits_for_the_active_run_and_then_runs_in_session() {
        let scratch = Scratch::new("idle-purge");
        let mut store = owing(&scratch);
        store
            .begin_turn(ThreadId(2), Some("kept"), RunId(2), "busy")
            .expect("an active run");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Busy);
        assert_eq!(owed(&scratch), 1);
        assert!(thread.send(StoreOp::EndRun {
            thread: ThreadId(2),
            run: RunId(2),
            text: None,
            revealed: None,
            end: RunEnd::Done,
        }));
        assert_eq!(next(&attempts), Attempt::Done);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn a_purge_a_reader_blocked_is_retried_in_session() {
        let scratch = Scratch::new("blocked-purge");
        let mut store = owing(&scratch);
        let reader = Connection::open(scratch.db()).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM events;")
            .expect("snapshot");
        // A write past the reader's snapshot, so the WAL can't be emptied under it.
        store
            .begin_turn(ThreadId(2), Some("later"), RunId(2), "later")
            .expect("turn");
        store
            .end_run(ThreadId(2), RunId(2), &[], RunEnd::Done)
            .expect("ended");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Blocked);
        drop(reader);
        // A retry racing the reader's close can find its file still held.
        done_after(&attempts, &[Attempt::Blocked, Attempt::Failed]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn a_failed_purge_is_retried_in_session() {
        let scratch = Scratch::new("failed-purge");
        let store = owing(&scratch);
        // The copy carries the trigger, so clearing its owed row fails.
        store
            .conn_for_tests()
            .execute_batch(
                "CREATE TRIGGER keep_owed BEFORE DELETE ON purge_pending BEGIN
                     SELECT RAISE(ABORT, 'kept');
                 END;",
            )
            .expect("trigger");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Failed);
        Connection::open(scratch.db())
            .expect("fixer")
            .execute_batch("DROP TRIGGER keep_owed")
            .expect("drop");
        done_after(&attempts, &[Attempt::Failed]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn every_operation_gets_one_reply_in_order() {
        let scratch = Scratch::new("replies");
        let store = Store::open(&scratch.db()).expect("store");
        let (thread, run) = (ThreadId(1), RunId(1));
        let (acks, mut replies) = mpsc::unbounded_channel();
        let store = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
        for op in [
            StoreOp::BeginTurn {
                thread,
                title: Some("t".to_owned()),
                run,
                text: "hi".to_owned(),
            },
            StoreOp::CommitStep {
                thread: ThreadId(2),
                run,
                text: None,
                index: 0,
                step: Step {
                    label: "s".to_owned(),
                },
            },
            StoreOp::EndRun {
                thread,
                run,
                text: None,
                revealed: None,
                end: RunEnd::Done,
            },
        ] {
            assert!(store.send(op));
        }
        store.close().blocking_recv().expect("closed");
        let mut got = Vec::new();
        while let Ok(reply) = replies.try_recv() {
            got.push(reply);
        }
        assert_eq!(
            got,
            [
                Input::TurnStored { run },
                Input::StoreFailed { run },
                Input::RunStored { run },
            ]
        );
    }
}
