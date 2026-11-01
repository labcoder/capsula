//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use tokio::sync::mpsc::UnboundedSender;

use crate::coordinator::{Input, StoreOp};
use crate::store::{EventKind, Store};

#[derive(Debug)]
pub(crate) struct StoreThread {
    ops: mpsc::Sender<StoreOp>,
    handle: JoinHandle<()>,
}

impl StoreThread {
    pub(crate) fn spawn(mut store: Store, acks: UnboundedSender<Input>) -> std::io::Result<Self> {
        let (ops, queue) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("store".to_owned())
            .spawn(move || {
                for op in queue {
                    if let Some(ack) = apply(&mut store, op)
                        && acks.send(ack).is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self { ops, handle })
    }

    /// Queues an operation. False when the thread has gone, which only a panic there causes.
    pub(crate) fn send(&self, op: StoreOp) -> bool {
        self.ops.send(op).is_ok()
    }

    /// Lets the queued operations finish, then closes the store.
    pub(crate) fn close(self) {
        drop(self.ops);
        if self.handle.join().is_err() {
            tracing::error!("the store thread panicked");
        }
    }
}

/// Runs one operation. Errors are logged by kind and run id only, never content.
fn apply(store: &mut Store, op: StoreOp) -> Option<Input> {
    match op {
        StoreOp::BeginTurn {
            thread,
            title,
            run,
            text,
        } => Some(
            match store.begin_turn(thread, title.as_deref(), run, &text) {
                Ok(()) => Input::TurnStored { run },
                Err(error) => {
                    tracing::warn!(%error, run = run.0, "the turn wasn't stored");
                    Input::StoreFailed { run }
                }
            },
        ),
        StoreOp::Append {
            thread,
            run,
            kind,
            text,
            body,
        } => store
            .append(thread, run, kind, text.as_deref(), body.as_deref())
            .err()
            .map(|error| {
                tracing::warn!(%error, run = run.0, "an event wasn't stored");
                Input::StoreFailed { run }
            }),
        StoreOp::CommitStep {
            thread,
            run,
            index,
            step,
        } => {
            let body = serde_json::json!({ "index": index, "label": step.label }).to_string();
            Some(
                match store.append(
                    thread,
                    run,
                    EventKind::SceneStep,
                    Some(&step.label),
                    Some(&body),
                ) {
                    Ok(()) => Input::StepStored { run, index, step },
                    Err(error) => {
                        tracing::warn!(%error, run = run.0, "a step wasn't stored");
                        Input::StoreFailed { run }
                    }
                },
            )
        }
        StoreOp::EndRun { run, end } => {
            if let Err(error) = store.end_run(run, end) {
                tracing::warn!(%error, run = run.0, "a run's outcome wasn't stored");
            }
            None
        }
    }
}
