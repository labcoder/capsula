//! A run's streaming half: the engine's events become text frames and committed steps in the
//! engine's order, and a round's end becomes the run's. The run's text counter and deadline
//! belong to the run, not to a round.

use std::fmt;

use kumochi_protocol::message::{
    DataToShell, MAX_STEP_LABEL, MAX_STEPS, Phase, RunId, SceneId, Step,
};

use super::{
    Coordinator, Effect, MAX_DELTA, MAX_TEXT, Notify, Run, Stale, StoreOp, run_state, unstored,
};
use crate::engines::{CallId, Finish, ToolName, TurnEvent};
use crate::store::{Failure, Interruption, RunEnd, SceneTarget};

/// The draw call a run's steps stream in, one step per closed line of its arguments.
pub(super) struct DrawArgs {
    call: CallId,
    /// Every argument byte so far.
    streamed: String,
    /// Where the line still open starts.
    line: usize,
    ready: bool,
}

impl fmt::Debug for DrawArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrawArgs")
            .field("call", &self.call)
            .field("streamed_bytes", &self.streamed.len())
            .field("line", &self.line)
            .field("ready", &self.ready)
            .finish()
    }
}

impl Coordinator {
    pub(super) fn engine(
        &mut self,
        run: RunId,
        event: TurnEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let drawing_pending = self.drawing.pending_for(run);
        let current = self.current(run)?;
        // The engine hasn't started before its turn is stored, and has nothing more after Done.
        if !current.turn_stored || current.finishing {
            return Err(Stale);
        }
        match event {
            TurnEvent::TextDelta(delta) => self.text(run, &delta, effects)?,
            TurnEvent::ToolCallStarted {
                call,
                name: ToolName::Draw,
            } if current.draw.is_none() => {
                current.draw = Some(DrawArgs {
                    call,
                    streamed: String::new(),
                    line: 0,
                    ready: false,
                });
            }
            // shortcut: one draw call per run and no other tool, since nothing answers a call;
            // the tool loop (slice 3's PR 13b) answers every call.
            TurnEvent::ToolCallStarted { .. } => self.fail(run, Failure::Engine, effects)?,
            TurnEvent::ToolArgsDelta { call, delta } => {
                self.draw_delta(run, &call, &delta, effects)?;
            }
            TurnEvent::ToolCallReady { call, args } => {
                self.draw_ready(run, &call, &args, effects)?;
            }
            // Native items have no reader until the tool loop stores them for replay.
            TurnEvent::OutputItem { .. } => {}
            // shortcut: a completed round ends the run, draw call and all, which only the fixture
            // adapter's draw-only rounds rely on; the tool loop answers the call and goes on.
            TurnEvent::Finished(Finish::Completed)
                if current.acked < current.steps || drawing_pending =>
            {
                current.finishing = true;
            }
            TurnEvent::Finished(Finish::Completed) => {
                let done = self.take(run)?;
                self.end(done, RunEnd::Done, None, Notify::Shell, effects);
            }
            // Only the coordinator interrupts a run, so an engine's interruption fails it too.
            TurnEvent::Finished(Finish::Truncated | Finish::Failed(_) | Finish::Interrupted) => {
                self.fail(run, Failure::Engine, effects)?;
            }
        }
        Ok(())
    }

    /// More of the answer: sent in frames that fit the data pipe, or held behind a pending step.
    fn text(&mut self, run: RunId, delta: &str, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let current = self.current(run)?;
        if current.text_bytes + delta.len() > MAX_TEXT {
            return self.fail(run, Failure::Budget, effects);
        }
        current.text_bytes += delta.len();
        current.unstored.push_str(delta);
        answering(current, effects);
        let mut rest = delta;
        while !rest.is_empty() {
            let mut cut = rest.len().min(MAX_DELTA);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let (piece, tail) = rest.split_at(cut);
            let frame = || DataToShell::AssistantText {
                run,
                delta: piece.to_owned(),
            };
            // Held text joins the frame before it, so small deltas don't each cost an entry.
            match current.held.back_mut() {
                Some(behind) => match behind.last_mut() {
                    Some(DataToShell::AssistantText { delta, .. })
                        if delta.len() + piece.len() <= MAX_DELTA =>
                    {
                        delta.push_str(piece);
                    }
                    _ => behind.push(frame()),
                },
                None => effects.push(Effect::Data(frame())),
            }
            rest = tail;
        }
        Ok(())
    }

    /// Each closed line of the draw call's arguments is one step. The open line is charged
    /// against the label cap before it grows, so arguments with no newline fail at the cap.
    fn draw_delta(
        &mut self,
        run: RunId,
        call: &CallId,
        delta: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let open = self.current(run)?.draw.as_ref();
        if !open.is_some_and(|draw| draw.call == *call && !draw.ready) {
            return self.fail(run, Failure::Engine, effects);
        }
        let mut pieces = delta.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            // A step over the run's budget ended it.
            let Ok(current) = self.current(run) else {
                return Ok(());
            };
            let draw = current
                .draw
                .as_mut()
                .expect("the draw call was checked open");
            if draw.streamed.len() - draw.line + piece.len() > MAX_STEP_LABEL {
                return self.fail(run, Failure::Engine, effects);
            }
            draw.streamed.push_str(piece);
            if pieces.peek().is_none() {
                break;
            }
            let label = draw.streamed[draw.line..].to_owned();
            draw.streamed.push('\n');
            draw.line = draw.streamed.len();
            self.step(run, Step { label }, effects)?;
        }
        Ok(())
    }

    /// The draw call's whole arguments must be what streamed; a last line with no newline is one
    /// more step.
    fn draw_ready(
        &mut self,
        run: RunId,
        call: &CallId,
        args: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let Some(draw) = self
            .current(run)?
            .draw
            .as_mut()
            .filter(|draw| draw.call == *call && !draw.ready && draw.streamed == args)
        else {
            return self.fail(run, Failure::Engine, effects);
        };
        draw.ready = true;
        let streamed = std::mem::take(&mut draw.streamed);
        let tail = &streamed[draw.line..];
        if tail.is_empty() {
            return Ok(());
        }
        self.step(
            run,
            Step {
                label: tail.to_owned(),
            },
            effects,
        )
    }

    fn fail(
        &mut self,
        run: RunId,
        failure: Failure,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let failed = self.take(run)?;
        self.end(
            failed,
            RunEnd::Failed(failure),
            None,
            Notify::Shell,
            effects,
        );
        Ok(())
    }

    /// An engine step: committed in order, or failing the run when it's over a limit. A run's
    /// first step begins its scene; the steps after it wait for the scene's id.
    fn step(&mut self, run: RunId, step: Step, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let current = self.current(run)?;
        let failure = if step.label.len() > MAX_STEP_LABEL {
            Some(Failure::Engine)
        } else if usize::try_from(current.steps).is_ok_and(|steps| steps >= MAX_STEPS) {
            Some(Failure::Budget)
        } else {
            None
        };
        if let Some(failure) = failure {
            let failed = self.take(run)?;
            self.end(
                failed,
                RunEnd::Failed(failure),
                None,
                Notify::Shell,
                effects,
            );
            return Ok(());
        }
        answering(current, effects);
        let evict = if current.steps == 0 {
            self.evict()
        } else {
            None
        };
        let current = self.current(run)?;
        let (text, index) = (unstored(current), current.steps);
        current.steps += 1;
        current.held.push_back(Vec::new());
        let scene = match current.scene {
            Some(scene) => SceneTarget::Existing(scene),
            None if index == 0 => {
                current.evict = evict;
                SceneTarget::New { evict }
            }
            // The scene's id comes with the first step's ack; the core loop never waits.
            None => {
                current.waiting.push_back((text, index, step));
                return Ok(());
            }
        };
        effects.push(Effect::Store(StoreOp::CommitStep {
            thread: current.thread,
            run,
            text,
            scene,
            index,
            step,
        }));
        Ok(())
    }

    /// The store acknowledges steps in order, so anything but the next pending index is stale.
    /// The first ack gives the run its scene and sends the steps that waited for it.
    pub(super) fn step_stored(
        &mut self,
        run: RunId,
        index: u32,
        step: Step,
        scene: SceneId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let found = self.find(run)?;
        if !found.turn_stored || index != found.acked || index >= found.sent() {
            return Err(Stale);
        }
        found.acked += 1;
        let first = found.scene.is_none();
        let scene = *found.scene.get_or_insert(scene);
        let (thread, evict) = (found.thread, found.evict.take());
        // Late acks after a Stop count in the revision but never come back into the kept set.
        let stopped = found
            .ending
            .as_ref()
            .is_some_and(|ending| ending.outcome == RunEnd::Interrupted(Interruption::Stop));
        let ending = found.ending.is_some();
        let waiting = std::mem::take(&mut found.waiting);
        if first {
            self.begin_scene(scene, (thread, run), evict);
        }
        self.scene_stored(scene, (!stopped).then_some(&step));
        if ending {
            self.settle(run, effects);
            return Ok(());
        }
        effects.push(Effect::Data(DataToShell::StepCommit {
            run,
            scene,
            index,
            step,
        }));
        let found = self.find(run)?;
        effects.extend(
            found
                .held
                .pop_front()
                .into_iter()
                .flatten()
                .map(Effect::Data),
        );
        for (text, index, step) in waiting {
            effects.push(Effect::Store(StoreOp::CommitStep {
                thread,
                run,
                text,
                scene: SceneTarget::Existing(scene),
                index,
                step,
            }));
        }
        if found.finishing && found.acked == found.steps && !self.drawing.pending_for(run) {
            let done = self.take(run)?;
            self.end(done, RunEnd::Done, None, Notify::Shell, effects);
        }
        Ok(())
    }
}

fn answering(run: &mut Run, effects: &mut Vec<Effect>) {
    if !run.answering {
        run.answering = true;
        effects.push(run_state(run, Phase::Answering));
    }
}
