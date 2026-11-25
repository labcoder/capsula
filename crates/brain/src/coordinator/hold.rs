//! Recognition-time admission data; the runtime alone owns the pending hold.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kumochi_protocol::message::{
    ConnectionKind, Content, ControlToShell, GestureId, HeldScreen, RunOrigin, ScreenClass,
    ThreadId,
};

use super::availability::Arrived;
use super::{Coordinator, Effect, Input, StoreOp, admission};
use crate::context::{Origin, RunSnapshot, Screen};
use crate::recall::control::{Control, READ_TIME};
use crate::store::{Generation, UnsentScreen};

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
    /// At the run's first request: tells the shell once what its hold's screen carried, unless it
    /// carried an image, which waits for the grant since none of it leaves before that.
    pub(super) fn tell_screen_unless_image(&mut self, effects: &mut Vec<Effect>) {
        let image = self.screen.as_ref().map(crate::context::Screen::held);
        if !matches!(
            image,
            Some(
                kumochi_protocol::message::HeldScreen::Attached
                    | kumochi_protocol::message::HeldScreen::Screen
            )
        ) {
            self.tell_screen(effects);
        }
    }

    /// The picture still to be told, if this run has a held picture to authorize for a send.
    pub(super) fn carried(&self) -> Option<super::lease::Carried> {
        let hold = self.hold.as_ref()?.key;
        let outcome = self.screen.as_ref()?.held();
        self.untold?;
        if !matches!(outcome, HeldScreen::Attached | HeldScreen::Screen) {
            return None;
        }
        Some(super::lease::Carried {
            run: self.id,
            hold,
            outcome,
            thread: self.thread,
        })
    }

    /// Whether a send of the run may have written its picture: a grant took the held image and no
    /// settlement has proved that send unsent.
    pub(super) fn exposed(&self) -> bool {
        self.untold.is_none() && matches!(self.screen, Some(Screen::Image(_) | Screen::Picture(..)))
    }

    /// Why the run ends when its picture may no longer go after a send may have written it.
    pub(super) fn picture_stopped(&self) -> kumochi_protocol::message::UserError {
        match self.screen_lost {
            Some(super::Lost::Generation) => kumochi_protocol::message::UserError::new(
                kumochi_protocol::message::ErrorKey::ScreenConnectionChanged,
                Vec::new(),
                vec![kumochi_protocol::message::Action::OpenSettings],
            )
            .expect("one action is within the limit"),
            _ => kumochi_protocol::message::ErrorKey::ScreenPictureStopped.into(),
        }
    }

    /// The run's picture may no longer go to its model. Before any send could have written it,
    /// the request goes without it, and the model, the shell and history get the existing
    /// `ModelCantSee` lines, never `Withdrawn`, which says the user turned sharing off. Once one
    /// may have, the picture stays, so the run's next build fails closed.
    pub(super) fn picture_lost(&mut self, effects: &mut Vec<Effect>) {
        if self.untold.is_none() {
            return;
        }
        let screen = match self.screen {
            Some(Screen::Image(_)) => Screen::NotIncluded(ScreenClass::ModelCantSee),
            Some(Screen::Picture(..)) => Screen::PictureNotIncluded(ScreenClass::ModelCantSee),
            _ => return,
        };
        self.screen = Some(screen);
        self.progress.unsent_screen = Some(UnsentScreen::ModelCantSee);
        match &mut self.begin {
            Some(StoreOp::BeginTurn { screen, .. }) => {
                *screen = Some(HeldScreen::NotIncluded(ScreenClass::ModelCantSee));
            }
            _ => effects.push(Effect::Store(StoreOp::CantSeeScreen { run: self.id })),
        }
        self.tell_screen(effects);
    }

    /// Tells the shell what its hold's screen carried, once.
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
    /// Withdraws every run's image that `allowed` says the setting turned off allowed, so each
    /// image follows its own setting: the run's own copy drops at once, and the bytes are
    /// zeroed when the last copy drops. The run's next build fails, and a request that has started
    /// keeps its copy until its round ends but is refused at the send. A turn whose image no
    /// granted send has carried yet is recorded as withdrawn; once one has, the turn keeps what it
    /// carried, unless that send wrote none of it.
    pub(super) fn screen_holds_off(
        &mut self,
        allowed: fn(&crate::context::Screen) -> bool,
        effects: &mut Vec<Effect>,
    ) {
        for run in self.ending.iter_mut().chain(self.current.as_mut()) {
            if !run.screen.as_ref().is_some_and(allowed) {
                continue;
            }
            run.screen = Some(crate::context::Screen::Withdrawn);
            if run.untold.take().is_none() {
                continue;
            }
            run.progress.unsent_screen = Some(UnsentScreen::Withdrawn);
            match &mut run.begin {
                Some(StoreOp::BeginTurn { screen, .. }) => {
                    *screen = Some(kumochi_protocol::message::HeldScreen::Withdrawn);
                }
                _ => effects.push(Effect::Store(StoreOp::WithdrawScreen { run: run.id })),
            }
        }
        self.revoke_pictures();
        if let Some(run) = self.current.as_ref().filter(|run| {
            matches!(run.screen, Some(crate::context::Screen::Withdrawn))
                && (run.key_granted || self.code_picture_grants(run.id).next().is_some())
        }) {
            self.fail_with(
                run.id,
                crate::store::Failure::Engine,
                kumochi_protocol::message::ErrorKey::ScreenUnavailable.into(),
                effects,
            )
            .expect("the run is current");
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
        // The store sets it again once a pinned choice resolves; native items replay under it.
        defaults.connection = arrived.replay(defaults.choice.as_ref());
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
        now.differs_beyond_pictures(self.availability_of(kind))
            && hold.is_none_or(|hold| hold.kind() == kind)
    }

    /// Whether `ChatGPT` becoming ready on `ready`, recovered with `boundary`, fails a pending
    /// hold: anything but the installed cutoff on its own unchanged sign-in counts as a change.
    pub(crate) fn hold_recovery_changes(
        &self,
        ready: &super::Ready,
        boundary: crate::store::Boundary,
        hold: Option<&HoldSnapshot>,
    ) -> bool {
        let kind = ConnectionKind::ChatGpt;
        hold.is_none_or(|hold| hold.kind() == kind)
            && !self.chatgpt_retired.contains(&ready.connection)
            && (self.recovered(ready.connection) != Some(boundary)
                || super::Availability::Ready(ready.clone())
                    .differs_beyond_pictures(self.availability_of(kind)))
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
            Input::RecoveredChatGpt { ready, boundary } => {
                hold.kind() == kumochi_protocol::message::ConnectionKind::ChatGpt
                    && !self.chatgpt_retired.contains(&ready.connection)
                    && (hold.arrived.get(hold.kind()) != Some(ready.connection)
                        || hold.arrived.recovered() != Some(*boundary))
            }
            Input::Engines(
                kind,
                super::Availability::Unavailable(_) | super::Availability::Finding,
            ) => *kind == hold.kind(),
            Input::ShellLost => true,
            _ => false,
        };
        if invalid {
            hold.cancel.store(true, Ordering::SeqCst);
        }
    }
}
