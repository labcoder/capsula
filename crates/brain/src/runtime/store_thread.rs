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
                // Every queued op runs, even with nobody left to hear the acknowledgement.
                for op in queue {
                    if acks.send(apply(&mut store, op)).is_err() {
                        tracing::debug!("an acknowledgement had no listener");
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
                events.push((EventKind::AssistantText, Some(text.as_str()), None));
            }
            events.push((
                EventKind::SceneStep,
                Some(step.label.as_str()),
                Some(body.as_str()),
            ));
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
                events.push((EventKind::AssistantText, Some(text.as_str()), None));
            }
            if let Some(revealed) = &revealed {
                events.push((EventKind::Revealed, None, Some(revealed.as_str())));
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
