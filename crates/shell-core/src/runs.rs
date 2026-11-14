//! The runs as the shell shows them: which one is moving, which turns Esc has cancelled before
//! their run was named, which runs it has fenced off, and the newest one's answer. Their steps
//! go to the [`Drawing`]. Plain state, no GPUI, so every rule is unit-tested.
//!
//! The fence is checked when a message is applied, not when it's received (architecture v1
//! §3.3), so messages already queued in the shell when Esc fences their run are dropped too.
//! Data for a run no `RunState` has named yet is held, with its credit, until one does, so a run
//! Esc cancelled before it was named never shows anything.

use std::collections::VecDeque;
use std::fmt;

mod hold;
use hold::Hold;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    Action, Content, ControlToBrain, DataToBrain, DataToShell, Deleted, ErrorKey, GestureId,
    Incarnation, MAX_PENDING_DELETES, MAX_PENDING_TURNS, Phase, RunId, RunOrigin, SceneDescriptor,
    SceneId, ThreadId, ThreadKind, ThreadRow, UserError,
};

#[cfg(any(test, feature = "test-support"))]
use crate::drawing::Scene;
use crate::drawing::{Broken, Chunk, Drawing, Effect};

/// The most streamed answer text kept until history replaces it, from the end [guess].
const TEXT_KEPT: usize = 4096;
/// Room in a data frame for a `TextSubmit`'s other fields [guess].
const SUBMIT_OVERHEAD: usize = 64;
/// Sign-in failures that may restore a submission before its run starts answering.
const RECONNECT: [ErrorKey; 6] = [
    ErrorKey::SessionEnded,
    ErrorKey::AccountRefused,
    ErrorKey::SignInNeeded,
    ErrorKey::ClaudeCodeSignedOut,
    ErrorKey::AnthropicKeyRejected,
    ErrorKey::RenewalUnconfirmed,
];

/// Why a turn wasn't sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Nothing but whitespace: the brain ignores it, so it would never be answered.
    Blank,
    /// Longer than a data frame can carry.
    TooLong,
    /// The turn queue is full, or a New target still awaits its first run state.
    Busy,
    /// No brain to send it to.
    Offline,
}

/// The newest run on screen.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Shown {
    pub run: RunId,
    /// Its thread, when its run was the live one as it began showing.
    pub thread: Option<ThreadId>,
    /// The end of its answer, at most [`TEXT_KEPT`] bytes.
    pub text: String,
    pub cut: bool,
    pub done: bool,
}

impl fmt::Debug for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shown")
            .field("run", &self.run)
            .field("thread", &self.thread)
            .field("text_len", &self.text.len())
            .field("cut", &self.cut)
            .field("done", &self.done)
            .finish()
    }
}

/// What applying a `RunState` asks of the caller.
#[derive(Default, PartialEq, Eq)]
pub struct Applied {
    /// A `Stop` for a run Esc cancelled before it was named.
    pub stop: Option<ControlToBrain>,
    /// Credit for held data frames this state classified.
    pub credit: u32,
    /// The run isn't fenced off, by Esc or a delete, so its feedback may show.
    pub visible: bool,
    /// The text of a turn that failed before answering, given back once for an empty
    /// composer.
    pub restore: Option<Content>,
}

impl fmt::Debug for Applied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Applied")
            .field("stop", &self.stop)
            .field("credit", &self.credit)
            .field("visible", &self.visible)
            .field("restore_len", &self.restore.as_ref().map(|text| text.len()))
            .finish()
    }
}

/// A turn sent and not answered yet, then its named run's until that run's first terminal state.
struct Submitted {
    /// The selection it was typed at.
    revision: u64,
    /// The number it was sent under on this connection.
    turn: u32,
    /// The thread it continues; a New target's is the one its run names.
    thread: Option<ThreadId>,
    /// What was typed, until nothing may put it back.
    text: Option<Content>,
    answering: bool,
}

impl fmt::Debug for Submitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Submitted")
            .field("revision", &self.revision)
            .field("turn", &self.turn)
            .field("thread", &self.thread)
            .field("text_len", &self.text.as_ref().map(|text| text.len()))
            .field("answering", &self.answering)
            .finish()
    }
}

/// The newest named run, until both its terminal `RunState` and its `RunEnd` have been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Live {
    run: RunId,
    thread: ThreadId,
    /// The turn number and selection of the submission it answered, when it answered one.
    typed: Option<(u32, u64)>,
    state_ended: bool,
    data_ended: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Target {
    #[default]
    Kumo,
    Task(ThreadId),
    New,
}

#[derive(Default)]
pub struct Runs {
    connected: bool,
    hold: Option<Hold>,
    hold_high: u32,
    hold_cancel: Option<GestureId>,
    turns_sent: u32,
    /// The newest named run while its output may still arrive.
    live: Option<Live>,
    /// The highest run Esc stopped; anything of it or older is dropped when applied.
    fence: RunId,
    /// The highest run any `RunState` named; only its first state can bind a typed turn.
    answered: RunId,
    /// Turns sent and not answered yet.
    submitted: VecDeque<Submitted>,
    /// The newest named run's submission, until that run's first terminal `RunState`.
    named: Option<(RunId, Submitted)>,
    recovery: Option<(RunId, Submitted)>,
    /// How many of the oldest submitted turns Esc cancelled.
    cancelled: usize,
    /// Data frames, with their size, for runs not named yet. Their credit is withheld, so the
    /// brain's credit window bounds them.
    held: Vec<(DataToShell, u32)>,
    /// The thread the next turn continues.
    target: Target,
    main: Option<ThreadId>,
    revision: u64,
    /// The target is known to be stored: picked from the store's list, or answered by a run, which
    /// starts only once its turn is stored.
    durable: bool,
    shown: Option<Shown>,
    drawing: Drawing,
    /// The brain the last connection reached.
    incarnation: Option<Incarnation>,
    /// Threads confirmed deleted whose delete isn't answered yet.
    deleting: Vec<ThreadId>,
    /// Threads hidden or removed while turns sent before were unanswered, with how many still
    /// are. Each holds one of the slots `deleting` shares.
    gone: Vec<(ThreadId, usize)>,
}

impl fmt::Debug for Runs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runs")
            .field("connected", &self.connected)
            .field("hold", &self.hold)
            .field("turns_sent", &self.turns_sent)
            .field("live", &self.live)
            .field("fence", &self.fence)
            .field("answered", &self.answered)
            .field("submitted", &self.submitted)
            .field("named", &self.named)
            .field("recovery", &self.recovery)
            .field("cancelled", &self.cancelled)
            .field("held", &self.held.len())
            .field("target", &self.target)
            .field("main", &self.main)
            .field("revision", &self.revision)
            .field("durable", &self.durable)
            .field("shown", &self.shown)
            .field("incarnation", &self.incarnation)
            .field("deleting", &self.deleting)
            .field("gone", &self.gone)
            .finish_non_exhaustive()
    }
}

impl Runs {
    pub fn select_row(&mut self, row: &ThreadRow) -> bool {
        if row.kind == ThreadKind::Main {
            self.back_to_kumo();
            true
        } else {
            self.select_thread(Some(row.id))
        }
    }

    /// Selects the thread the next typed turn continues; no message or store write is needed.
    /// Refuses, changing nothing, a thread that holds a delete slot, so the caller leaves the
    /// open thread alone too.
    pub fn select_thread(&mut self, thread: Option<ThreadId>) -> bool {
        if thread.is_some_and(|thread| self.holds(thread)) {
            return false;
        }
        self.cancel_hold();
        self.target = match thread {
            Some(thread) if Some(thread) == self.main => Target::Kumo,
            Some(thread) => Target::Task(thread),
            None => Target::New,
        };
        self.durable = thread.is_some();
        self.revision = self.revision.wrapping_add(1);
        self.forget_texts(|_| true);
        true
    }

    pub fn target(&self) -> Target {
        self.target
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The lengths of the typed texts still held: pending turns oldest first, then the named run's.
    #[cfg(any(test, feature = "test-support"))]
    pub fn kept(&self) -> Vec<usize> {
        self.submitted
            .iter()
            .chain(self.named.as_ref().map(|(_, submitted)| submitted))
            .chain(self.recovery.as_ref().map(|(_, submitted)| submitted))
            .filter_map(|submitted| submitted.text.as_ref().map(|text| text.len()))
            .collect()
    }

    pub fn settings(&mut self, main: ThreadId) {
        self.main = Some(main);
    }

    pub fn back_to_kumo(&mut self) {
        self.cancel_hold();
        self.target = Target::Kumo;
        self.durable = true;
        self.revision = self.revision.wrapping_add(1);
        self.forget_texts(|_| true);
    }

    /// Drops the text of every submission, pending or named, whose captured thread `owned`
    /// accepts, so nothing can put it back.
    fn forget_texts(&mut self, owned: impl Fn(Option<ThreadId>) -> bool) {
        if self
            .recovery
            .as_ref()
            .is_some_and(|(_, submitted)| owned(submitted.thread))
        {
            self.recovery = None;
        }
        for submitted in &mut self.submitted {
            if owned(submitted.thread) {
                submitted.text = None;
            }
        }
        if self
            .named
            .as_ref()
            .is_some_and(|(_, submitted)| owned(submitted.thread))
        {
            self.named = None;
        }
    }

    pub fn thread(&self) -> Option<ThreadId> {
        match self.target {
            Target::Kumo => self.main,
            Target::Task(thread) => Some(thread),
            Target::New => None,
        }
    }

    fn capture(&self, text: &str) -> Result<(DataToBrain, u32, u64), Refused> {
        if !self.connected || self.main.is_none() {
            return Err(Refused::Offline);
        }
        if self.target == Target::New
            && self
                .submitted
                .iter()
                .any(|submitted| submitted.revision == self.revision)
        {
            return Err(Refused::Busy);
        }
        let turn = self.turns_sent.checked_add(1).ok_or(Refused::Busy)?;
        Ok((
            DataToBrain::TextSubmit {
                turn,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: self.thread(),
                text: text.into(),
            },
            turn,
            self.revision,
        ))
    }

    /// A run is moving that Esc hasn't stopped, or a turn is waiting for its run.
    pub fn working(&self) -> bool {
        self.hold.is_some() || self.stoppable().is_some() || self.submitted.len() > self.cancelled
    }

    /// Steps are on screen, or a recovery Esc could cancel is under way.
    pub fn drawing(&self) -> bool {
        self.drawing.active()
    }

    /// The scenes on screen, oldest first.
    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only this crate's tests read it")
    )]
    pub(crate) fn scenes(&self) -> impl Iterator<Item = &Scene> {
        self.drawing.shown().map(|(_, scene)| scene)
    }

    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    pub fn retry_ready(&self, run: RunId) -> bool {
        self.recovery.as_ref().is_some_and(|(owner, submitted)| {
            *owner == run
                && submitted.text.is_some()
                && submitted.revision == self.revision
                && submitted.thread == self.thread()
        })
    }

    pub fn dismiss_retry(&mut self) {
        self.recovery = None;
    }

    pub fn retry(
        &mut self,
        run: RunId,
        draft: &str,
        send: impl FnOnce(DataToBrain) -> bool,
    ) -> Result<(), Refused> {
        let Some((_, submitted)) = self.recovery.take_if(|(owner, _)| *owner == run) else {
            return Err(Refused::Busy);
        };
        if !draft.is_empty()
            || submitted.revision != self.revision
            || submitted.thread != self.thread()
        {
            return Err(Refused::Busy);
        }
        let text = submitted.text.ok_or(Refused::Busy)?;
        self.submit(&text, send)
    }

    fn stoppable(&self) -> Option<RunId> {
        self.live
            .map(|live| live.run)
            .filter(|run| *run > self.fence)
    }

    /// The first Esc: cancels every turn sent and not yet answered, fences the moving run off,
    /// keeping what's on screen (a placeholder step has no entrance, so every one shown has
    /// finished entering), and returns the `Stop` to send after. A cancelled turn's `Stop`
    /// comes from [`Runs::apply_state`] once its run is named.
    pub fn stop(&mut self) -> Option<ControlToBrain> {
        let run = self.stoppable();
        self.cancel_hold();
        self.forget_texts(|_| true);
        self.cancelled = self.submitted.len();
        let run = run?;
        self.fence = run;
        Some(ControlToBrain::Stop {
            run,
            revealed: self.drawing.revealed(run),
        })
    }

    /// The second Esc, or Esc while nothing moves: clears the drawing, recovery included, and
    /// returns the `ClearDrawing` to send after. The text stays.
    pub fn clear(&mut self) -> Effect {
        self.cancel_hold();
        self.forget_texts(|_| true);
        self.drawing.clear()
    }

    /// The connection's `Scenes`.
    ///
    /// # Errors
    /// [`Broken`] when the list breaks the protocol.
    pub fn apply_scenes(&mut self, scenes: &[SceneDescriptor]) -> Result<Effect, Broken> {
        self.drawing.apply_scenes(scenes)
    }

    /// A snapshot chunk from connection `generation`; returns the credit to grant now.
    ///
    /// # Errors
    /// [`Broken`] when the chunk breaks the protocol.
    pub fn apply_snapshot(&mut self, generation: u64, chunk: Chunk) -> Result<u32, Broken> {
        self.drawing.apply_snapshot(generation, chunk)
    }

    /// Sends a typed turn through `send`, which says whether it went, unless it would be refused.
    /// Every attempt, refused or not, ends any earlier turn's chance to have its text put back.
    ///
    /// # Errors
    /// [`Refused`] says why it wasn't sent.
    pub fn submit(
        &mut self,
        text: &str,
        send: impl FnOnce(DataToBrain) -> bool,
    ) -> Result<(), Refused> {
        self.cancel_hold();
        self.forget_texts(|_| true);
        if text.trim().is_empty() {
            return Err(Refused::Blank);
        }
        if text.len() > Pipe::Data.max_payload() - SUBMIT_OVERHEAD {
            return Err(Refused::TooLong);
        }
        let (message, turn, revision) = self.capture(text)?;
        if self.submitted.len() + 1 >= MAX_PENDING_TURNS {
            return Err(Refused::Busy);
        }
        if self.hold_cancel.is_some() {
            return Err(Refused::Busy);
        }
        if !send(message) {
            return Err(Refused::Offline);
        }
        self.submitted.push_back(Submitted {
            revision,
            turn,
            thread: self.thread(),
            text: Some(text.into()),
            answering: false,
        });
        self.turns_sent = turn;
        Ok(())
    }

    /// A run's `Focus`. It moves the target, as an explicit switch does, only while `run` is
    /// the newest named run and not stopped, answers a turn typed at the current selection, and
    /// no turn was sent after that one on this connection, even one the brain then refused.
    pub fn focus(&mut self, run: RunId, thread: ThreadId) -> bool {
        let current = self.connected
            && run > self.fence
            && run == self.answered
            && self.live.is_some_and(|live| {
                live.run == run && live.typed == Some((self.turns_sent, self.revision))
            });
        current && self.select_thread(Some(thread))
    }

    /// The brain this connection reached. A new one can reuse the run id of a turn the last
    /// one never stored, so nothing the last one numbered may fence or match its runs.
    pub fn met(&mut self, incarnation: Incarnation) {
        if self.incarnation.replace(incarnation) == Some(incarnation) {
            return;
        }
        self.cancel_hold();
        self.hold_cancel = None;
        self.answered = RunId(0);
        self.fence = RunId(0);
        self.forget_texts(|_| true);
        // A new thread whose first turn the last brain never stored doesn't exist for this one.
        // shortcut: a new thread stopped or failed before answering loses its target here even
        // when its turn was stored; a stored-turn ack in RunState would keep it.
        if !self.durable {
            self.back_to_kumo();
        }
        // The answer stays on screen, but run 0 is none of the new brain's runs.
        if let Some(shown) = &mut self.shown {
            shown.run = RunId(0);
            shown.thread = None;
            shown.done = false;
        }
        self.drawing.forget_runs();
    }

    /// A delete slot is free.
    pub fn can_delete(&self) -> bool {
        self.deleting.len() + self.gone.len() < MAX_PENDING_DELETES
    }

    /// `thread` holds a delete slot: its delete is unanswered, or turns sent before it are.
    pub(crate) fn holds(&self, thread: ThreadId) -> bool {
        self.deleting.contains(&thread) || self.gone.iter().any(|(gone, _)| *gone == thread)
    }

    /// `holds`, for the shell's tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn holds_for_tests(&self, thread: ThreadId) -> bool {
        self.holds(thread)
    }

    /// The threads whose delete is unanswered.
    pub fn deleting(&self) -> &[ThreadId] {
        &self.deleting
    }

    /// The user confirmed deleting `thread`: it takes a slot and returns input to Kumo if
    /// this was the target. False when every slot is held or it already holds one.
    pub fn confirm_delete(&mut self, thread: ThreadId) -> bool {
        if !self.can_delete() || self.holds(thread) {
            return false;
        }
        self.deleting.push(thread);
        if self.hold.is_some_and(|hold| hold.thread == Some(thread)) {
            self.cancel_hold();
        }
        self.forget_texts(|owner| owner == Some(thread));
        if self.target == Target::Task(thread) {
            self.back_to_kumo();
        }
        true
    }

    /// The brain answered `thread`'s delete. On a hide or removal, every run up to `last_run`
    /// is fenced before the thread's text goes, and held frames the fence now covers are dropped
    /// with their credit, which this returns. An answer for a thread with no slot is ignored.
    pub fn deleted(&mut self, thread: ThreadId, outcome: Deleted, last_run: Option<RunId>) -> u32 {
        let Some(at) = self.deleting.iter().position(|held| *held == thread) else {
            return 0;
        };
        self.deleting.remove(at);
        if outcome == Deleted::NotHidden {
            return 0;
        }
        self.fence = self.fence.max(last_run.unwrap_or_default());
        let fence = self.fence;
        let mut credit = 0_u32;
        self.held.retain(|(message, bytes)| {
            let fenced = run_of(message).is_some_and(|run| run <= fence);
            if fenced {
                credit = credit.saturating_add(*bytes);
            }
            !fenced
        });
        if !self.submitted.is_empty() {
            self.gone.push((thread, self.submitted.len()));
        }
        // A view with no known owner began under a superseded run, so it may be this thread's.
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.thread.is_none_or(|owner| owner == thread))
        {
            self.shown = None;
        }
        if self.live.is_some_and(|live| live.thread == thread) {
            self.live = None;
        }
        credit
    }

    /// The brain refused the identified pending turn before giving it a run. It settles only the
    /// counts: nothing is shown, stopped, fenced or targeted.
    pub fn refused(&mut self, turn: u32, thread: Option<ThreadId>) {
        let Some(at) = self
            .submitted
            .iter()
            .position(|submitted| submitted.turn == turn && submitted.thread == thread)
        else {
            return;
        };
        if self.submitted.remove(at).is_some() {
            self.answered_one(at);
        }
    }

    /// Only prefixes containing the removed pending entry wait on one fewer.
    fn answered_one(&mut self, at: usize) {
        if at < self.cancelled {
            self.cancelled -= 1;
        }
        for (_, waiting) in &mut self.gone {
            if at < *waiting {
                *waiting -= 1;
            }
        }
        self.gone.retain(|(_, waiting)| *waiting > 0);
    }

    /// A deleted thread's scenes left the visible set; returns the credit of held chunks dropped.
    pub fn remove_scenes(&mut self, scenes: &[SceneId]) -> u32 {
        self.drawing.remove(scenes)
    }

    /// `Scenes` has been applied on this connection, so resent deletes follow it.
    pub fn described(&self) -> bool {
        self.drawing.described()
    }

    /// The link connected, as connection `generation`.
    pub fn connected(&mut self, generation: u64) {
        self.connected = true;
        self.turns_sent = 0;
        self.drawing.connected(generation);
    }

    /// The link lost the brain: nothing of the runs it knew can arrive any more, and the turns
    /// it hadn't answered never will be.
    pub fn disconnected(&mut self) {
        self.cancel_hold();
        self.hold_cancel = None;
        self.connected = false;
        if let Some(shown) = &mut self.shown {
            shown.thread = None;
            shown.done = false;
        }
        self.fence = self.fence.max(self.answered);
        self.live = None;
        self.main = None;
        self.submitted.clear();
        self.named = None;
        self.recovery = None;
        self.cancelled = 0;
        self.held.clear();
        self.gone.clear();
        self.drawing.disconnected();
    }

    /// A `RunState`. A run newer than any before answers only its matching typed turn once,
    /// fenced or not, and is fenced before any of its output is applied if Esc cancelled that
    /// turn. Only a pending turn at the current revision can adopt a New target's thread.
    /// A `RunState` with the brain's reason: what applying it asks, and the notice to show, which
    /// is withheld once Esc or a delete has fenced the run off. A failure that means the turn
    /// needs sign-in restores its text once before answering, only while its thread and selection are
    /// current.
    pub fn apply_run_state(
        &mut self,
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        error: Option<UserError>,
        origin: Option<RunOrigin>,
    ) -> (Applied, Option<UserError>) {
        let applied = self.apply(run, thread, phase, error.as_ref(), origin);
        let notice = error.filter(|_| applied.visible);
        (applied, notice)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn apply_state(
        &mut self,
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        origin: Option<RunOrigin>,
    ) -> Applied {
        self.apply(run, thread, phase, None, origin)
    }

    fn apply(
        &mut self,
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        error: Option<&UserError>,
        origin: Option<RunOrigin>,
    ) -> Applied {
        let mut applied = Applied::default();
        self.apply_hold_state(run, thread, origin, &mut applied);
        if run > self.answered {
            self.answered = run;
            // A turn sent before its thread's delete never targets it, and shows nothing.
            let deleted = self.holds(thread);
            if deleted {
                self.fence = self.fence.max(run);
            }
            let mut typed = None;
            if let Some(RunOrigin::Typed { turn }) = origin
                && let Some(at) = self.submitted.iter().position(|submitted| {
                    submitted.turn == turn && submitted.thread.is_none_or(|owner| owner == thread)
                })
                && let Some(mut submitted) = self.submitted.remove(at)
            {
                typed = Some((submitted.turn, submitted.revision));
                if submitted.thread.is_none()
                    && submitted.revision == self.revision
                    && self.target == Target::New
                    && !deleted
                {
                    self.target = if Some(thread) == self.main {
                        Target::Kumo
                    } else {
                        Target::Task(thread)
                    };
                }
                let cancelled = at < self.cancelled;
                self.answered_one(at);
                if cancelled {
                    self.fence = self.fence.max(run);
                    applied.stop = Some(ControlToBrain::Stop {
                        run,
                        revealed: Vec::new(),
                    });
                }
                submitted.thread = Some(thread);
                self.named = (run > self.fence).then_some((run, submitted));
            }
            if run > self.fence {
                self.live = Some(Live {
                    run,
                    thread,
                    typed,
                    state_ended: false,
                    data_ended: false,
                });
            }
        }
        if matches!(
            phase,
            Phase::Answering | Phase::Done | Phase::Interrupted | Phase::Failed
        ) && self.hold.is_some_and(|hold| hold.run == Some(run))
        {
            self.hold = None;
        }
        if phase == Phase::Answering
            && let Some((_, submitted)) = self.named.as_mut().filter(|(named, _)| *named == run)
        {
            submitted.answering = true;
        }
        if matches!(phase, Phase::Answering | Phase::Done) && self.thread() == Some(thread) {
            self.durable = true;
        }
        if phase == Phase::Done
            && self
                .live
                .is_some_and(|live| live.run == run && !live.state_ended)
            && let Some(shown) = self.view(run)
        {
            shown.thread = Some(thread);
            shown.done = true;
        }
        if matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed) {
            self.end(run, |live| live.state_ended = true);
            // The first terminal state uses the submission up, so a duplicate finds nothing.
            if let Some((_, submitted)) = self.named.take_if(|(named, _)| *named == run)
                && phase == Phase::Failed
                && run > self.fence
                && submitted.revision == self.revision
                && submitted.thread == Some(thread)
                && self.thread() == Some(thread)
            {
                if !submitted.answering
                    && error.is_some_and(|error| RECONNECT.contains(&error.key()))
                {
                    applied.restore = submitted.text;
                } else if error.is_some_and(|error| error.actions().contains(&Action::Retry)) {
                    self.recovery = Some((run, submitted));
                }
            }
        }
        applied.credit = self.release();
        applied.visible = run > self.fence;
        applied
    }

    /// A data frame of `bytes` on the pipe. Returns the credit to grant for it now: none while
    /// it's held for its run's first `RunState`.
    pub fn apply_data(&mut self, message: DataToShell, bytes: u32) -> u32 {
        if run_of(&message).is_some_and(|run| run > self.answered) {
            self.held.push((message, bytes));
            return 0;
        }
        self.show(message);
        bytes
    }

    /// Applies the held frames whose run is named now, in arrival order, and returns their credit.
    fn release(&mut self) -> u32 {
        let answered = self.answered;
        let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(message, _)| run_of(message).is_none_or(|run| run <= answered));
        self.held = held;
        ready.into_iter().fold(0, |credit, (message, bytes)| {
            self.show(message);
            credit.saturating_add(bytes)
        })
    }

    /// A newer run takes over the view, and anything fenced, cleared or older than the view is
    /// dropped.
    fn show(&mut self, message: DataToShell) {
        match message {
            DataToShell::AssistantText { run, delta } => {
                if let Some(shown) = self.view(run) {
                    shown.text.push_str(&delta);
                    shown.cut |= keep_end(&mut shown.text, TEXT_KEPT);
                    self.answered_from(run);
                }
            }
            DataToShell::StepCommit {
                run,
                scene,
                index,
                step,
            } => {
                if self.view(run).is_some() {
                    self.drawing.step(run, scene, index, step);
                    self.answered_from(run);
                }
            }
            DataToShell::RunEnd { run } => self.end(run, |live| live.data_ended = true),
            DataToShell::IssueCard(_)
            | DataToShell::IssueCardRead { .. }
            | DataToShell::IssueCardsDone { .. }
            | DataToShell::IssueReview { .. }
            | DataToShell::ThreadList { .. }
            | DataToShell::Page { .. }
            | DataToShell::PageFailed { .. }
            | DataToShell::SceneSnapshot { .. }
            | DataToShell::ScenesRemoved { .. }
            | DataToShell::ScenesReplaced { .. }
            | DataToShell::SceneBegin { .. }
            | DataToShell::SceneStepCommit { .. }
            | DataToShell::SceneSnapshotV1 { .. }
            | DataToShell::SceneCutApplied { .. }
            | DataToShell::SceneEnd { .. }
            | DataToShell::Models { .. }
            | DataToShell::Transcript(_)
            | DataToShell::SegmentQueued { .. }
            | DataToShell::SpeechPcm(_) => {
                // shortcut: Presence consumes v1 frames through Presenter first; B3 removes placeholder routing.
                // shortcut: the catalog is credited and dropped until PR 18b's picker shows it.
                // shortcut: voice frames are credited and dropped until slice 7's PR 6 plays them.
            }
        }
    }

    /// Output of the live run of the target thread proves the thread stored, since a run's
    /// engine starts only once its turn is; an older run's output proves nothing.
    fn answered_from(&mut self, run: RunId) {
        if self
            .live
            .is_some_and(|live| live.run == run && self.thread() == Some(live.thread))
        {
            self.durable = true;
        }
    }

    /// Marks one of `run`'s two endings; the live run settles once both are in, and an older
    /// run's ending never touches a newer one.
    fn end(&mut self, run: RunId, mark: impl FnOnce(&mut Live)) {
        if let Some(live) = self.live.as_mut().filter(|live| live.run == run) {
            mark(live);
            if live.state_ended && live.data_ended {
                self.live = None;
            }
        }
    }

    /// The view `run`'s data goes to, if it isn't fenced or older than the view.
    fn view(&mut self, run: RunId) -> Option<&mut Shown> {
        if run <= self.fence || self.shown.as_ref().is_some_and(|shown| run < shown.run) {
            return None;
        }
        if self.shown.as_ref().is_none_or(|shown| run > shown.run) {
            let thread = self
                .live
                .filter(|live| live.run == run)
                .map(|live| live.thread);
            self.shown = Some(Shown {
                run,
                thread,
                ..Shown::default()
            });
        }
        self.shown.as_mut()
    }
}

fn run_of(message: &DataToShell) -> Option<RunId> {
    match message {
        DataToShell::AssistantText { run, .. }
        | DataToShell::StepCommit { run, .. }
        | DataToShell::RunEnd { run }
        | DataToShell::SegmentQueued { run, .. } => Some(*run),
        DataToShell::Transcript(transcript) => Some(transcript.run),
        DataToShell::SpeechPcm(pcm) => Some(pcm.run),
        DataToShell::ThreadList { .. }
        | DataToShell::Page { .. }
        | DataToShell::PageFailed { .. }
        | DataToShell::SceneSnapshot { .. }
        | DataToShell::ScenesRemoved { .. }
        | DataToShell::ScenesReplaced { .. }
        | DataToShell::SceneBegin { .. }
        | DataToShell::SceneStepCommit { .. }
        | DataToShell::SceneSnapshotV1 { .. }
        | DataToShell::SceneCutApplied { .. }
        | DataToShell::SceneEnd { .. } => {
            // shortcut: Presence consumes v1 frames through Presenter first; B3 removes placeholder routing.
            None
        }
        DataToShell::Models { .. }
        | DataToShell::IssueCard(_)
        | DataToShell::IssueCardRead { .. }
        | DataToShell::IssueCardsDone { .. }
        | DataToShell::IssueReview { .. } => None,
    }
}

/// Drops the start of `text` so at most `max` bytes stay, cutting at a character boundary.
fn keep_end(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
    true
}

#[cfg(test)]
mod delete_tests;

#[cfg(test)]
mod focus_tests;

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "runs_hold_tests.rs"]
mod hold_tests;
