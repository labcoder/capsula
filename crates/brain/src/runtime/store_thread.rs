//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::coordinator::{Input, StoreOp};
use crate::store::{EventKind, NewEvent, Purge, Queue, Store};

/// How long the queue stays empty before an owed purge starts [guess].
const QUIET: Duration = Duration::from_millis(200);
/// How long a purge waits after a reader blocked its checkpoint [guess].
const RETRY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct StoreThread {
    ops: mpsc::Sender<StoreOp>,
    /// Counts the queued ops, so a purge gives way to them.
    queue: Queue,
    /// Fires when the thread has run every queued op and closed the store.
    done: oneshot::Receiver<()>,
}

impl StoreThread {
    pub(crate) fn spawn(mut store: Store, acks: UnboundedSender<Input>) -> std::io::Result<Self> {
        let (ops_tx, ops) = mpsc::channel();
        let (finished, done) = oneshot::channel();
        let queue = store.queue();
        let taken = queue.clone();
        thread::Builder::new()
            .name("store".to_owned())
            .spawn(move || {
                // Every queued op runs, even with nobody left to hear the acknowledgement.
                // shortcut: in a debug build a panic here leaves queued ops unanswered, so their runs
                // never leave the coordinator and new turns stop once 16 pile up; release builds
                // abort on panic instead. Restart the brain on a store panic once it has a supervisor.
                // An owed purge, from a deletion or found at start, runs once the queue is quiet.
                let mut next_purge = store.purge_pending().then(Instant::now);
                loop {
                    let op = match next_purge {
                        None => ops.recv().ok(),
                        // An owed purge waits for a quiet queue, so it doesn't start mid-run.
                        Some(at) => match ops
                            .recv_timeout(at.saturating_duration_since(Instant::now()).max(QUIET))
                        {
                            Ok(op) => Some(op),
                            Err(RecvTimeoutError::Timeout) => {
                                next_purge = purge(&mut store);
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => None,
                        },
                    };
                    let Some(op) = op else {
                        break;
                    };
                    taken.pop();
                    if acks.send(apply(&mut store, op)).is_err() {
                        tracing::debug!("an acknowledgement had no listener");
                    }
                }
                drop(store);
                if finished.send(()).is_err() {
                    tracing::debug!("nobody waited for the store to close");
                }
            })?;
        Ok(Self {
            ops: ops_tx,
            queue,
            done,
        })
    }

    /// Queues an operation, interrupting a purge so the op doesn't wait behind it. False when
    /// the thread has gone, which only a panic there causes.
    pub(crate) fn send(&self, op: StoreOp) -> bool {
        self.queue.push(|| self.ops.send(op).is_ok())
    }

    /// Lets the queued operations finish, then closes the store; a running purge stops, and the
    /// next start runs it. The receiver fires when it has, and fails if the thread panicked;
    /// nothing blocks on the thread itself.
    pub(crate) fn close(self) -> oneshot::Receiver<()> {
        let Self { ops, queue, done } = self;
        queue.push(|| drop(ops));
        done
    }
}

/// Tries an owed purge; when to try again, or `None` once nothing is owed or it failed. A failed
/// purge waits for the next start, so a full disk isn't rewritten every second.
fn purge(store: &mut Store) -> Option<Instant> {
    match store.purge() {
        Ok(Purge::Done) => None,
        // The op it gave way to is already queued, so the next try waits for quiet again.
        Ok(Purge::Yielded) => Some(Instant::now()),
        Ok(Purge::Blocked) => Some(Instant::now() + RETRY),
        Err(error) => {
            tracing::error!(%error, "the purge failed; the next start retries it");
            None
        }
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
    use std::time::{Duration, Instant};

    use capsula_protocol::message::{RunId, Step, ThreadId};
    use rusqlite::Connection;
    use tokio::sync::mpsc;

    use super::StoreThread;
    use crate::coordinator::{Input, StoreOp};
    use crate::store::{Deletion, RunEnd, Store};
    use crate::test_support::Scratch;

    /// A store with its one thread hidden for deletion.
    fn deleting(scratch: &Scratch) -> Store {
        let mut store = Store::open(&scratch.db()).expect("store");
        store
            .begin_turn(ThreadId(1), Some("t"), RunId(1), "doomed")
            .expect("turn");
        store.begin_delete(ThreadId(1)).expect("begin");
        store
    }

    fn finish(store: &mut Store) {
        assert_eq!(
            store.finish_delete(ThreadId(1)).expect("finish"),
            Deletion::Pending
        );
    }

    /// Waits for the store's thread to finish the owed purge, with nothing sent to it.
    fn purged_in_session(scratch: &Scratch, store: Store) {
        let (acks, _replies) = mpsc::unbounded_channel();
        let thread = StoreThread::spawn(store, acks).expect("thread");
        let watcher = Connection::open(scratch.db()).expect("watcher");
        let deadline = Instant::now() + Duration::from_secs(10);
        let owed = || -> i64 {
            watcher
                .query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))
                .expect("count")
        };
        while owed() > 0 {
            assert!(Instant::now() < deadline, "the purge is still owed");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(watcher);
        thread.close().blocking_recv().expect("closed");
    }

    #[test]
    fn a_purge_that_gave_way_finishes_once_the_queue_is_quiet() {
        let scratch = Scratch::new("quiet-purge");
        let mut store = deleting(&scratch);
        store.queue().push(|| ());
        finish(&mut store);
        store.queue().pop();
        purged_in_session(&scratch, store);
    }

    #[test]
    fn a_purge_a_reader_blocked_is_retried_in_session() {
        let scratch = Scratch::new("blocked-purge");
        let mut store = deleting(&scratch);
        let reader = Connection::open(scratch.db()).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM events;")
            .expect("snapshot");
        finish(&mut store);
        drop(reader);
        purged_in_session(&scratch, store);
    }

    #[test]
    fn every_operation_gets_one_reply_in_order() {
        let scratch = Scratch::new("replies");
        let store = Store::open(&scratch.db()).expect("store");
        let (thread, run) = (ThreadId(1), RunId(1));
        let (acks, mut replies) = mpsc::unbounded_channel();
        let store = StoreThread::spawn(store, acks).expect("thread");
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
