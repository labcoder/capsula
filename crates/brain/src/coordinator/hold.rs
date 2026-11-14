//! Recognition-time admission data; the runtime alone owns the pending hold.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kumochi_protocol::message::{
    ConnectionKind, Content, ControlToShell, GestureId, RunOrigin, ThreadId,
};

use super::availability::Arrived;
use super::{Coordinator, Effect, Input, StoreOp, admission};
use crate::context::{ConnectionTag, Origin, RunSnapshot};
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
    /// The sign-in each kind had ready when the hold began; the hold answers on its choice's.
    pub(crate) arrived: Arrived,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl HoldSnapshot {
    /// The kind the hold answers on: its choice's, else `ChatGPT`'s.
    fn kind(&self) -> ConnectionKind {
        self.defaults
            .choice
            .as_ref()
            .map_or(ConnectionKind::ChatGpt, |choice| choice.connection)
    }
}

impl fmt::Debug for HoldSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HoldSnapshot")
            .field("key", &self.key)
            .field("thread", &self.thread)
            .field("selection", &self.selection)
            .field("generation", &self.generation)
            .field("connection", &self.defaults.connection)
            .field("arrived", &self.arrived)
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
            && self.arrived == other.arrived
    }
}
impl Eq for HoldSnapshot {}

impl super::Run {
    /// At the run's first request: tells the shell what its hold's screen carried, once.
    pub(super) fn tell_screen(&mut self, effects: &mut Vec<Effect>) {
        let (Some(gesture), Some(screen)) = (self.untold.take(), &self.screen) else {
            return;
        };
        effects.push(Effect::Control(ControlToShell::HoldScreen {
            gesture,
            outcome: screen.held(),
        }));
    }
}

impl Coordinator {
    /// Withdraws every run's image: the run's own copy drops at once, and the bytes are zeroed
    /// when the last copy drops. The run's next build fails, and a request that has started
    /// keeps its copy until its round ends but is refused at the send. A turn whose image no
    /// request has carried yet is recorded as withdrawn; once a request has started with it, the
    /// turn keeps `attached`.
    pub(super) fn screen_holds_off(&mut self, effects: &mut Vec<Effect>) {
        for run in self.ending.iter_mut().chain(self.current.as_mut()) {
            if !matches!(run.screen, Some(crate::context::Screen::Image(_))) {
                continue;
            }
            run.screen = Some(crate::context::Screen::Withdrawn);
            if run.untold.take().is_none() {
                continue;
            }
            match &mut run.begin {
                Some(StoreOp::BeginTurn { screen, .. }) => {
                    *screen = Some(kumochi_protocol::message::HeldScreen::Withdrawn);
                }
                _ => effects.push(Effect::Store(StoreOp::WithdrawScreen { run: run.id })),
            }
        }
    }

    /// Captures the sign-in each kind has ready, without allocating a run or a new thread.
    pub(crate) fn prepare_hold(
        &self,
        key: HoldKey,
        thread: Option<ThreadId>,
        selection: u64,
        mut defaults: RunSnapshot,
        cancel: Arc<AtomicBool>,
    ) -> Result<StoreOp, ()> {
        let arrived = self.arrived_now();
        if cancel.load(Ordering::SeqCst)
            || arrived.is_empty()
            || thread.is_some_and(|thread| self.deleting_thread(thread))
        {
            cancel.store(true, Ordering::SeqCst);
            return Err(());
        }
        // Read only to replay native items, which only a `ChatGPT` route does.
        defaults.connection = arrived
            .get(ConnectionKind::ChatGpt)
            .unwrap_or(ConnectionTag(0));
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
                arrived,
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
            screen,
            &mut effects,
        );
        // shortcut: resent text can't be told from a fresh turn, so the shell offers no Retry for
        // a held run; one that does must name its run and fail with ScreenUnavailable when that
        // run carried a screen.
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
            || !self.answers(snapshot))
    }

    /// Whether `hold` can still answer: its kind's sign-in from when it began is still ready, and
    /// that kind's catalog lists its choice. A `ChatGPT` hold also replays under that sign-in.
    fn answers(&self, hold: &HoldSnapshot) -> bool {
        let choice = hold.defaults.choice.as_ref();
        self.route_for(hold.arrived, choice).is_ok_and(|route| {
            (route.kind != ConnectionKind::ChatGpt || route.tag == hold.defaults.connection)
                && self.refusal(route).is_none()
                && self.unlisted(route.kind, choice).is_none()
        })
    }

    pub(crate) fn cancel_current_hold(&mut self) -> Vec<Effect> {
        let key = self
            .current
            .as_ref()
            .and_then(|run| run.hold.as_ref())
            .map(|hold| hold.key);
        key.map_or_else(Vec::new, |key| self.cancel_hold(key))
    }

    /// Whether `kind` becoming `now` fails a pending hold: a real change to the hold's own kind, or
    /// to any kind while its read, and so its kind, is still out.
    pub(crate) fn hold_availability_changes(
        &self,
        kind: ConnectionKind,
        now: &super::Availability,
        hold: Option<&HoldSnapshot>,
    ) -> bool {
        now != self.availability_of(kind) && hold.is_none_or(|hold| hold.kind() == kind)
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
                    && !self.deleting_thread(run.thread)
                    && run.snapshot.as_ref() == Some(&hold.defaults)
                    && self.answers(hold)
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
            Input::Engines(kind, super::Availability::Ready(ready)) => {
                *kind == hold.kind() && hold.arrived.get(*kind) != Some(ready.connection)
            }
            Input::Engines(kind, super::Availability::Unavailable(_)) => *kind == hold.kind(),
            Input::ShellLost => true,
            _ => false,
        };
        if invalid {
            hold.cancel.store(true, Ordering::SeqCst);
        }
    }
}
