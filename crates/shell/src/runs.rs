//! The runs as the shell shows them: which one is moving, which turns Esc has cancelled before
//! their run was named, which runs it has fenced off, and the newest one's answer. Their steps
//! go to the [`Drawing`]. Plain state, no GPUI, so every rule is unit-tested.
//!
//! The fence is checked when a message is applied, not when it's received (architecture v1
//! §3.3), so messages already queued in the shell when Esc fences their run are dropped too.
//! Data for a run no `RunState` has named yet is held, with its credit, until one does, so a run
//! Esc cancelled before it was named never shows anything.

use std::collections::VecDeque;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, Deleted, Incarnation, MAX_PENDING_DELETES,
    MAX_PENDING_TURNS, Phase, RunId, SceneDescriptor, SceneId, ThreadId, ThreadKind, ThreadRow,
};

use crate::drawing::{Broken, Chunk, Drawing, Effect, Scene};

/// The most answer text the placeholder keeps, from the end [guess].
const TEXT_KEPT: usize = 4096;
/// Room in a data frame for a `TextSubmit`'s other fields [guess].
const SUBMIT_OVERHEAD: usize = 64;

/// Why a turn wasn't sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
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
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Shown {
    pub(crate) run: RunId,
    /// Its thread, when its run was the live one as it began showing.
    pub(crate) thread: Option<ThreadId>,
    /// The end of its answer, at most [`TEXT_KEPT`] bytes.
    pub(crate) text: String,
}

/// What applying a `RunState` asks of the caller.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// A `Stop` for a run Esc cancelled before it was named.
    pub(crate) stop: Option<ControlToBrain>,
    /// Credit for held data frames this state classified.
    pub(crate) credit: u32,
}

/// The newest named run, until both its terminal `RunState` and its `RunEnd` have been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Live {
    run: RunId,
    thread: ThreadId,
    state_ended: bool,
    data_ended: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Target {
    #[default]
    Kumo,
    Task(ThreadId),
    New,
}

#[derive(Debug, Default)]
pub(crate) struct Runs {
    connected: bool,
    /// The newest named run while its output may still arrive.
    live: Option<Live>,
    /// The highest run Esc stopped; anything of it or older is dropped when applied.
    fence: RunId,
    /// The highest run any `RunState` named: each new one answers the oldest pending turn.
    answered: RunId,
    /// Turns sent and not answered yet.
    submitted: VecDeque<u64>,
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

impl Runs {
    pub(crate) fn select_row(&mut self, row: &ThreadRow) -> bool {
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
    pub(crate) fn select_thread(&mut self, thread: Option<ThreadId>) -> bool {
        if thread.is_some_and(|thread| self.holds(thread)) {
            return false;
        }
        self.target = match thread {
            Some(thread) if Some(thread) == self.main => Target::Kumo,
            Some(thread) => Target::Task(thread),
            None => Target::New,
        };
        self.durable = thread.is_some();
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(crate) fn target(&self) -> Target {
        self.target
    }

    #[cfg(test)]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn settings(&mut self, main: ThreadId) {
        self.main = Some(main);
    }

    pub(crate) fn back_to_kumo(&mut self) {
        self.target = Target::Kumo;
        self.durable = true;
        self.revision = self.revision.wrapping_add(1);
    }

    fn thread(&self) -> Option<ThreadId> {
        match self.target {
            Target::Kumo => self.main,
            Target::Task(thread) => Some(thread),
            Target::New => None,
        }
    }

    fn capture(&self, text: &str) -> Result<(DataToBrain, u64), Refused> {
        if !self.connected || self.main.is_none() {
            return Err(Refused::Offline);
        }
        if self.target == Target::New && self.submitted.contains(&self.revision) {
            return Err(Refused::Busy);
        }
        Ok((
            DataToBrain::TextSubmit {
                thread: self.thread(),
                text: text.to_owned(),
            },
            self.revision,
        ))
    }

    /// A run is moving that Esc hasn't stopped, or a turn is waiting for its run.
    pub(crate) fn working(&self) -> bool {
        self.stoppable().is_some() || self.submitted.len() > self.cancelled
    }

    /// Steps are on screen, or a recovery Esc could cancel is under way.
    pub(crate) fn drawing(&self) -> bool {
        self.drawing.active()
    }

    /// The scenes on screen, oldest first.
    pub(crate) fn scenes(&self) -> impl Iterator<Item = &Scene> {
        self.drawing.shown().map(|(_, scene)| scene)
    }

    pub(crate) fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
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
    pub(crate) fn stop(&mut self) -> Option<ControlToBrain> {
        self.cancelled = self.submitted.len();
        let run = self.stoppable()?;
        self.fence = run;
        Some(ControlToBrain::Stop {
            run,
            revealed: self.drawing.revealed(run),
        })
    }

    /// The second Esc, or Esc while nothing moves: clears the drawing, recovery included, and
    /// returns the `ClearDrawing` to send after. The text stays.
    pub(crate) fn clear(&mut self) -> Effect {
        self.drawing.clear()
    }

    /// The connection's `Scenes`.
    ///
    /// # Errors
    /// [`Broken`] when the list breaks the protocol.
    pub(crate) fn apply_scenes(&mut self, scenes: &[SceneDescriptor]) -> Result<Effect, Broken> {
        self.drawing.apply_scenes(scenes)
    }

    /// A snapshot chunk from connection `generation`; returns the credit to grant now.
    ///
    /// # Errors
    /// [`Broken`] when the chunk breaks the protocol.
    pub(crate) fn apply_snapshot(&mut self, generation: u64, chunk: Chunk) -> Result<u32, Broken> {
        self.drawing.apply_snapshot(generation, chunk)
    }

    /// Sends a typed turn through `send`, which says whether it went, unless it would be refused.
    ///
    /// # Errors
    /// [`Refused`] says why it wasn't sent.
    pub(crate) fn submit(
        &mut self,
        text: &str,
        send: impl FnOnce(DataToBrain) -> bool,
    ) -> Result<(), Refused> {
        if text.trim().is_empty() {
            return Err(Refused::Blank);
        }
        if text.len() > Pipe::Data.max_payload() - SUBMIT_OVERHEAD {
            return Err(Refused::TooLong);
        }
        let (turn, revision) = self.capture(text)?;
        if self.submitted.len() + 1 >= MAX_PENDING_TURNS {
            return Err(Refused::Busy);
        }
        if !send(turn) {
            return Err(Refused::Offline);
        }
        self.submitted.push_back(revision);
        Ok(())
    }

    /// The brain this connection reached. A new one can reuse the run id of a turn the last
    /// one never stored, so nothing the last one numbered may fence or match its runs.
    pub(crate) fn met(&mut self, incarnation: Incarnation) {
        if self.incarnation.replace(incarnation) == Some(incarnation) {
            return;
        }
        self.answered = RunId(0);
        self.fence = RunId(0);
        // A new thread whose first turn the last brain never stored doesn't exist for this one.
        // shortcut: a new thread stopped or failed before answering loses its target here even
        // when its turn was stored; a stored-turn ack in RunState would keep it.
        if !self.durable {
            self.back_to_kumo();
        }
        // The answer stays on screen, but run 0 is none of the new brain's runs.
        if let Some(shown) = &mut self.shown {
            shown.run = RunId(0);
        }
        self.drawing.forget_runs();
    }

    /// A delete slot is free.
    pub(crate) fn can_delete(&self) -> bool {
        self.deleting.len() + self.gone.len() < MAX_PENDING_DELETES
    }

    /// `thread` holds a delete slot: its delete is unanswered, or turns sent before it are.
    pub(crate) fn holds(&self, thread: ThreadId) -> bool {
        self.deleting.contains(&thread) || self.gone.iter().any(|(gone, _)| *gone == thread)
    }

    /// The threads whose delete is unanswered.
    pub(crate) fn deleting(&self) -> &[ThreadId] {
        &self.deleting
    }

    /// The user confirmed deleting `thread`: it takes a slot and returns input to Kumo if
    /// this was the target. False when every slot is held or it already holds one.
    pub(crate) fn confirm_delete(&mut self, thread: ThreadId) -> bool {
        if !self.can_delete() || self.holds(thread) {
            return false;
        }
        self.deleting.push(thread);
        if self.target == Target::Task(thread) {
            self.back_to_kumo();
        }
        true
    }

    /// The brain answered `thread`'s delete. On a hide or removal, every run up to `last_run`
    /// is fenced before the thread's text goes, and held frames the fence now covers are dropped
    /// with their credit, which this returns. An answer for a thread with no slot is ignored.
    pub(crate) fn deleted(
        &mut self,
        thread: ThreadId,
        outcome: Deleted,
        last_run: Option<RunId>,
    ) -> u32 {
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

    /// The brain refused the oldest pending turn before giving it a run. It settles only the
    /// counts: nothing is shown, stopped, fenced or targeted.
    pub(crate) fn refused(&mut self) {
        if self.submitted.pop_front().is_none() {
            return;
        }
        self.answered_one();
        self.cancelled = self.cancelled.saturating_sub(1);
    }

    /// One pending turn was answered: every `gone` entry waits on one fewer.
    fn answered_one(&mut self) {
        for (_, waiting) in &mut self.gone {
            *waiting -= 1;
        }
        self.gone.retain(|(_, waiting)| *waiting > 0);
    }

    /// A deleted thread's scenes left the visible set; returns the credit of held chunks dropped.
    pub(crate) fn remove_scenes(&mut self, scenes: &[SceneId]) -> u32 {
        self.drawing.remove(scenes)
    }

    /// `Scenes` has been applied on this connection, so resent deletes follow it.
    pub(crate) fn described(&self) -> bool {
        self.drawing.described()
    }

    /// The link connected, as connection `generation`.
    pub(crate) fn connected(&mut self, generation: u64) {
        self.connected = true;
        self.drawing.connected(generation);
    }

    /// The link lost the brain: nothing of the runs it knew can arrive any more, and the turns
    /// it hadn't answered never will be.
    pub(crate) fn disconnected(&mut self) {
        self.connected = false;
        self.fence = self.fence.max(self.answered);
        self.live = None;
        self.main = None;
        self.submitted.clear();
        self.cancelled = 0;
        self.held.clear();
        self.gone.clear();
        self.drawing.disconnected();
    }

    /// A `RunState`. A run newer than any before answers the oldest pending turn exactly once,
    /// fenced or not, and is fenced before any of its output is applied if Esc cancelled that
    /// turn. Only a pending turn at the current revision can adopt a New target's thread.
    pub(crate) fn apply_state(&mut self, run: RunId, thread: ThreadId, phase: Phase) -> Applied {
        let mut applied = Applied::default();
        if run > self.answered {
            self.answered = run;
            // A turn sent before its thread's delete never targets it, and shows nothing.
            let deleted = self.holds(thread);
            if deleted {
                self.fence = self.fence.max(run);
            }
            if let Some(revision) = self.submitted.pop_front() {
                if revision == self.revision && self.target == Target::New && !deleted {
                    self.target = if Some(thread) == self.main {
                        Target::Kumo
                    } else {
                        Target::Task(thread)
                    };
                }
                self.answered_one();
                if self.cancelled > 0 {
                    self.cancelled -= 1;
                    self.fence = self.fence.max(run);
                    applied.stop = Some(ControlToBrain::Stop {
                        run,
                        revealed: Vec::new(),
                    });
                }
            }
            if run > self.fence {
                self.live = Some(Live {
                    run,
                    thread,
                    state_ended: false,
                    data_ended: false,
                });
            }
        }
        if matches!(phase, Phase::Answering | Phase::Done) && self.thread() == Some(thread) {
            self.durable = true;
        }
        if matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed) {
            self.end(run, |live| live.state_ended = true);
        }
        applied.credit = self.release();
        applied
    }

    /// A data frame of `bytes` on the pipe. Returns the credit to grant for it now: none while
    /// it's held for its run's first `RunState`.
    pub(crate) fn apply_data(&mut self, message: DataToShell, bytes: u32) -> u32 {
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
                    keep_end(&mut shown.text, TEXT_KEPT);
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
            DataToShell::ThreadList { .. }
            | DataToShell::Page { .. }
            | DataToShell::PageFailed { .. }
            | DataToShell::SceneSnapshot { .. }
            | DataToShell::ScenesRemoved { .. }
            | DataToShell::SceneBegin { .. }
            | DataToShell::SceneStepCommit { .. }
            | DataToShell::SceneSnapshotV1 { .. }
            | DataToShell::SceneCutApplied { .. }
            | DataToShell::SceneEnd { .. } => {}
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
        | DataToShell::RunEnd { run } => Some(*run),
        DataToShell::ThreadList { .. }
        | DataToShell::Page { .. }
        | DataToShell::PageFailed { .. }
        | DataToShell::SceneSnapshot { .. }
        | DataToShell::ScenesRemoved { .. }
        | DataToShell::SceneBegin { .. }
        | DataToShell::SceneStepCommit { .. }
        | DataToShell::SceneSnapshotV1 { .. }
        | DataToShell::SceneCutApplied { .. }
        | DataToShell::SceneEnd { .. } => None,
    }
}

/// Drops the start of `text` so at most `max` bytes stay, cutting at a character boundary.
fn keep_end(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
}

#[cfg(test)]
mod delete_tests;

#[cfg(test)]
mod tests;
