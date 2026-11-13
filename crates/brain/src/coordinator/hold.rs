//! Recognition-time admission data; the runtime alone owns the pending hold.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kumochi_protocol::message::{Content, GestureId, RunOrigin, ThreadId};

use super::{Coordinator, Effect, Input, StoreOp, admission};
use crate::context::{Origin, RunSnapshot};
use crate::recall::control::{Control, READ_TIME};
use crate::store::Generation;

/// A gesture is unique only within its shell connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldKey {
    pub(crate) epoch: u64,
    pub(crate) gesture: GestureId,
}

/// One recognition-time read, ordered after previously accepted store writes.
#[derive(Clone, Debug)]
pub struct HoldRead {
    pub(crate) control: Control,
    pub(crate) snapshot: HoldSnapshot,
}

impl PartialEq for HoldRead {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot
    }
}
impl Eq for HoldRead {}

/// Pinned admission data, without audio or recognized words. Equality excludes cancellation.
#[derive(Clone)]
pub struct HoldSnapshot {
    pub(crate) key: HoldKey,
    pub(crate) thread: Option<ThreadId>,
    pub(crate) selection: u64,
    pub(crate) generation: Option<Generation>,
    pub(crate) defaults: RunSnapshot,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl fmt::Debug for HoldSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HoldSnapshot")
            .field("key", &self.key)
            .field("thread", &self.thread)
            .field("selection", &self.selection)
            .field("generation", &self.generation)
            .field("connection", &self.defaults.connection)
            .finish_non_exhaustive()
    }
}

impl PartialEq for HoldSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.thread == other.thread
            && self.selection == other.selection
            && self.generation == other.generation
            && self.defaults == other.defaults
    }
}
impl Eq for HoldSnapshot {}

impl Coordinator {
    /// Captures the ready connection, without allocating a run or a new thread.
    pub(crate) fn prepare_hold(
        &self,
        key: HoldKey,
        thread: Option<ThreadId>,
        selection: u64,
        mut defaults: RunSnapshot,
        cancel: Arc<AtomicBool>,
    ) -> Result<StoreOp, ()> {
        let connection = self.connection();
        if cancel.load(Ordering::SeqCst)
            || connection.is_none()
            || thread.is_some_and(|thread| self.deleting_thread(thread))
        {
            cancel.store(true, Ordering::SeqCst);
            return Err(());
        }
        defaults.connection = connection.ok_or(())?;
        defaults.origin = thread.map_or(Origin::Kumo, |thread| {
            admission::origin(thread, self.main_thread)
        });
        Ok(StoreOp::ReadHold(HoldRead {
            control: Control::new(Instant::now() + READ_TIME).observe(Arc::clone(&cancel)),
            snapshot: HoldSnapshot {
                key,
                thread,
                selection,
                generation: None,
                defaults,
                cancel,
            },
        }))
    }

    /// Consumes the runtime's single held admission without advancing typed/Ask watermarks.
    pub(crate) fn submit_hold(
        &mut self,
        snapshot: HoldSnapshot,
        text: Content,
        screen: Option<crate::context::Screen>,
    ) -> Result<Vec<Effect>, ()> {
        if text.trim().is_empty() || !self.admits_hold(&snapshot) {
            snapshot.cancel.store(true, Ordering::SeqCst);
            return Err(());
        }
        let mut effects = Vec::new();
        self.start_turn(
            RunOrigin::Hold {
                gesture: snapshot.key.gesture,
            },
            snapshot.thread,
            text,
            Some(snapshot),
            &mut effects,
        );
        // shortcut: the shell retries a failed run by sending its text as a new typed turn, so a
        // held run's retry goes without its screen and nothing says so; record that a run carried
        // a screen and fail its retry with ScreenUnavailable before 2c4 turns screen holds on.
        if let Some(run) = &mut self.current {
            run.screen = screen;
        }
        self.drawing_settle(&mut effects);
        self.retry_with(&mut effects);
        self.arm_retry(&mut effects);
        Ok(effects)
    }

    /// The current run's screen, for tests outside the coordinator.
    #[cfg(test)]
    pub(crate) fn current_screen(&self) -> Option<&crate::context::Screen> {
        self.current.as_ref().and_then(|run| run.screen.as_ref())
    }

    pub(crate) fn admits_hold(&self, snapshot: &HoldSnapshot) -> bool {
        !(snapshot.cancel.load(Ordering::SeqCst)
            || snapshot.thread.is_some() != snapshot.generation.is_some()
            || snapshot
                .thread
                .is_some_and(|thread| self.deleting_thread(thread))
            || self.connection() != Some(snapshot.defaults.connection)
            || self.unlisted(snapshot.defaults.choice.as_ref()).is_some())
    }

    pub(crate) fn cancel_current_hold(&mut self) -> Vec<Effect> {
        let key = self
            .current
            .as_ref()
            .and_then(|run| run.hold.as_ref())
            .map(|hold| hold.key);
        key.map_or_else(Vec::new, |key| self.cancel_hold(key))
    }

    pub(crate) fn hold_availability_changes(&self, now: &super::Availability) -> bool {
        now != &self.availability
    }

    /// Cancels only the real run transferred from this exact hold.
    pub(crate) fn cancel_hold(&mut self, key: HoldKey) -> Vec<Effect> {
        let Some(current) = self
            .current
            .as_ref()
            .filter(|run| run.hold.as_ref().is_some_and(|hold| hold.key == key))
        else {
            return Vec::new();
        };
        current
            .hold
            .as_ref()
            .expect("matched hold")
            .cancel
            .store(true, Ordering::SeqCst);
        self.handle(Input::Stop {
            run: current.id,
            revealed: Vec::new(),
        })
        .expect("the matched run is current")
    }

    pub(super) fn hold_valid(&self) -> bool {
        self.current.as_ref().is_none_or(|run| {
            run.hold.as_ref().is_none_or(|hold| {
                !hold.cancel.load(Ordering::SeqCst)
                    && self.connection() == Some(hold.defaults.connection)
                    && !self.deleting_thread(run.thread)
                    && run.snapshot.as_ref() == Some(&hold.defaults)
                    && self.unlisted(hold.defaults.choice.as_ref()).is_none()
            })
        })
    }

    pub(super) fn fence_hold_input(&self, input: &Input) {
        let Some(run) = &self.current else { return };
        let Some(hold) = &run.hold else { return };
        let invalid = match input {
            Input::Delete { thread, .. }
            | Input::ThreadOp(kumochi_protocol::message::ThreadOp::SetModel { thread, .. }) => {
                *thread == run.thread
            }
            Input::Engines(super::Availability::Ready(ready)) => {
                ready.connection != hold.defaults.connection
            }
            Input::Engines(super::Availability::Unavailable(_)) | Input::ShellLost => true,
            _ => false,
        };
        if invalid {
            hold.cancel.store(true, Ordering::SeqCst);
        }
    }
}
