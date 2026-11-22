//! V1 drawing effects share the coordinator's run fences, store FIFO and clear debt.

use kumochi_protocol::message::{
    DataToShell, Narration, RunId, SceneId, SegmentId, SegmentStart, StepRef, ThreadId,
};
use kumochi_scene::record::Start;

use super::{BuildKey, Coordinator, Effect, Stale, StoreOp, speech};
use crate::context::{Geometry, NoCanvas, Screen};
use crate::dispatch::{self, Dispatch, LedgerKey, ToolError, ToolOutput};
use crate::drawing::{Drawing, Input, Loss, Output, Reply, ScreenMarks, Write};

const MAX_RESULTS: usize = 64;

pub(super) struct StepReply {
    serial: u64,
    key: BuildKey,
    revision: u32,
    say: kumochi_scene::Text,
    call: Option<Box<str>>,
    segment: Option<SegmentId>,
}

impl Coordinator {
    pub(super) fn drawing_call(
        &mut self,
        thread: ThreadId,
        key: &LedgerKey,
        draw: dispatch::DrawAction,
        effects: &mut Vec<Effect>,
    ) -> Dispatch {
        let ended = Dispatch::Answered(ToolOutput::Error(ToolError::Ended));
        if !self.drawing_available() {
            self.drawn(key, ToolOutput::Error(ToolError::NotHere), effects);
            return Dispatch::Committing;
        }
        let current = self.current.as_mut().expect("the resolved run is current");
        let allocating = matches!(
            draw,
            dispatch::DrawAction::Complete(_) | dispatch::DrawAction::Start
        );
        if allocating
            && current.draw_calls.len()
                > usize::try_from(super::MAX_CALLS).expect("the call budget fits usize")
        {
            self.drawn(key, ToolOutput::Error(ToolError::TooLarge), effects);
            return Dispatch::Committing;
        }
        let call = if allocating {
            let call = crate::engines::CallId::new(&format!("draw-{}", current.draw_calls.len()))
                .expect("a bounded host-generated ASCII id");
            current.draw_calls.push(super::provider_draw::DrawCall {
                call: call.clone(),
                key: key.clone(),
                build: current.build.as_ref().map(super::admission::Build::key),
                provider: None,
            });
            call
        } else {
            let Some(call) = current
                .draw_calls
                .iter()
                .find(|call| call.key == *key)
                .map(|call| call.call.clone())
            else {
                return ended;
            };
            call
        };
        let input = match draw {
            dispatch::DrawAction::Complete(Ok(draw)) => crate::drawing::Input::Call {
                thread,
                run: key.run,
                call,
                args: draw.args,
            },
            dispatch::DrawAction::Complete(Err(error)) => crate::drawing::Input::Rejected {
                thread,
                run: key.run,
                call,
                error,
            },
            dispatch::DrawAction::Start => return Dispatch::Committing,
            dispatch::DrawAction::Record { index, bytes } => crate::drawing::Input::Record {
                thread,
                run: key.run,
                call,
                index,
                bytes,
            },
            dispatch::DrawAction::Finish(error) => crate::drawing::Input::CallFinished {
                thread,
                run: key.run,
                call,
                error,
            },
        };
        let recipient = self
            .current
            .as_ref()
            .and_then(|run| run.picture_recipient.clone());
        self.drawing.author(key.run, recipient.as_ref());
        if let Some(canvas) = self.canvas() {
            self.drawing.canvas(key.run, canvas, recipient.as_ref());
        }
        if self.drawing_input(input, effects).is_err() {
            self.drawn(key, ToolOutput::Error(ToolError::Ended), effects);
        }
        Dispatch::Committing
    }

    /// What a screen scene of the current run may be drawn on, asked as the request that sent
    /// its picture was; `None` while `draw` can't mark the screen.
    pub(super) fn canvas(&self) -> Option<Result<Geometry, NoCanvas>> {
        let current = self.current.as_ref()?;
        let choice = current
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.choice.as_ref());
        let pictured = self.request_pictured(current.route, choice);
        (self.screen_marks != ScreenMarks::Off)
            .then(|| Screen::canvas(current.screen.as_ref(), pictured))
    }

    pub(crate) fn drawing_described(&mut self) {
        self.drawing_held.clear();
    }

    pub(super) fn data_end(&mut self, run: RunId, thread: ThreadId, effects: &mut Vec<Effect>) {
        if let Some((_, _, end)) = self.drawing_held.iter_mut().rfind(|(owner, frame, _)| {
            *owner == thread
                && matches!(frame, DataToShell::SceneStepCommit { run: source, .. } | DataToShell::SceneEnd { run: source, .. } if *source == run)
        }) {
            *end = Some(run);
        } else {
            effects.push(Effect::Data(DataToShell::RunEnd { run }));
        }
    }

    pub(super) fn stop_drawings(
        &mut self,
        run: RunId,
        cuts: kumochi_protocol::message::SceneCuts,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if self.current_run() == Some(run) {
            let mut current = self.take(run)?;
            speech::revoke(&mut current, crate::voice::reply::Revoker::Shell, effects);
            self.end(
                current,
                crate::store::RunEnd::Interrupted(crate::store::Interruption::Stop),
                None,
                super::Notify::Shell,
                effects,
            );
        }
        self.drawing_input(Input::StopScenes { run, cuts }, effects)
    }

    /// Restores the valid visible v1 scenes and owes durable clears for the omitted scenes.
    pub fn restore_drawings(
        &mut self,
        recovery: Result<crate::store::DrawingRecovery, crate::store::StoreError>,
    ) -> Vec<Effect> {
        let Ok(recovery) = recovery else {
            tracing::warn!("stored drawing recovery was unavailable");
            self.drawing = Drawing::unavailable();
            return Vec::new();
        };
        let (drawing, mut skipped) = Drawing::recover(recovery.scenes);
        self.drawing = drawing;
        skipped.extend(recovery.skipped);
        skipped.sort_unstable();
        skipped.dedup();
        self.failed_drawings(skipped)
    }

    pub(crate) fn failed_drawings(&mut self, scenes: Vec<SceneId>) -> Vec<Effect> {
        let mut effects = Vec::new();
        for scene in &scenes {
            tracing::warn!(row = scene.0, "skipped while recovering drawing scene");
        }
        self.drawing_clear(scenes.clone(), &mut effects);
        let scenes: Vec<_> = scenes
            .into_iter()
            .filter(|scene| !self.clearing.iter().any(|(owed, _)| owed == scene))
            .collect();
        if !scenes.is_empty() {
            let output = self.drawing.clears_pending(scenes.len());
            self.drawing_output(output, &mut effects);
            self.drawing_output(
                Output {
                    effects: vec![Effect::Store(StoreOp::ClearScenes { scenes })],
                    calls: Vec::new(),
                },
                &mut effects,
            );
        }
        effects
    }

    pub(crate) fn drawing_available(&self) -> bool {
        self.drawing.available()
    }

    /// Lays drawing steps out through the runtime's preparation thread, as the shipped brain does.
    #[cfg(test)]
    pub(crate) fn defer_drawing_preparation(&mut self) {
        self.drawing.defer_preparation = true;
    }

    /// A frozen, validated v1 snapshot for the next connection's data FIFO.
    /// # Errors
    /// Rejects a visible set whose earlier writes have not settled.
    pub fn drawing_snapshot(
        &self,
    ) -> Result<crate::drawing::Snapshot, kumochi_protocol::message::SceneDataError> {
        if !self.drawing.settled() || !self.clearing.is_empty() || !self.keeping.is_empty() {
            return Err(kumochi_protocol::message::SceneDataError::Descriptor);
        }
        Ok(self.drawing.snapshot())
    }

    pub(super) fn drawing_input(
        &mut self,
        input: Input,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        match &input {
            Input::Call { thread, run, .. }
            | Input::Rejected { thread, run, .. }
            | Input::Record { thread, run, .. }
            | Input::CallFinished { thread, run, .. } => {
                let current = self.current(*run)?;
                if current.thread != *thread || !current.started() || current.finishing {
                    return Err(Stale);
                }
            }
            Input::StepShown { run, .. }
            | Input::StepReady { run, .. }
            | Input::StepFailed { run, .. }
                if *run < self.session_start =>
            {
                return Err(Stale);
            }
            _ => {}
        }
        let failed = match &input {
            Input::StepFailed { run, scene, .. } => Some((*run, *scene)),
            _ => None,
        };
        let output = self.drawing.handle(input)?;
        self.drawing_output(output, effects);
        if let Some((run, scene)) = failed {
            self.step_failed(run, scene, effects);
        }
        Ok(())
    }

    pub(super) fn drawing_output(&mut self, output: Output, effects: &mut Vec<Effect>) {
        for effect in output.effects {
            if let Effect::Data(message) = &effect
                && let Some(thread) = scene_id(message).and_then(|scene| self.drawing.thread(scene))
                && self.deleting_thread(thread)
            {
                if self.drawing_held.len() >= MAX_RESULTS {
                    effects.push(Effect::Broken("too many drawing frames await deletion"));
                } else if let Effect::Data(message) = effect {
                    self.drawing_held.push((thread, message, None));
                }
                continue;
            }
            if let Effect::RetireScenes { scenes, successor } = effect {
                let successor = successor.filter(|scene| {
                    !self
                        .drawing
                        .thread(*scene)
                        .is_some_and(|thread| self.deleting_thread(thread))
                });
                self.drawing_revoke(scenes, successor, effects);
                continue;
            }
            if let Effect::DrawingReplyText { key, text } = effect {
                self.release_reply_text(key, &text, effects);
                continue;
            }
            if let Effect::Store(StoreOp::ClearScenes { scenes }) = &effect {
                for scene in scenes {
                    if !self.clearing.iter().any(|(owed, _)| owed == scene) {
                        self.clearing.push((*scene, true));
                    }
                }
            }
            if let Effect::Store(StoreOp::Drawing { serial, write }) = &effect {
                self.reserve_step_reply(*serial, write, effects);
            }
            self.forward_drawing_effect(effect, effects);
        }
        for end in output.calls {
            // Codes and schema field paths only: the model's ids stay in its tool result.
            match &end.error {
                Some(crate::drawing::CallError::Step(error)) => {
                    tracing::warn!(code = %error.code, field = %error.path, "draw call refused");
                }
                Some(crate::drawing::CallError::Interrupted) | None => {}
                Some(error) => tracing::warn!(code = %error, "draw call stopped"),
            }
            let key = self
                .current
                .as_ref()
                .filter(|run| run.id == end.run)
                .and_then(|run| run.draw_calls.iter().find(|call| call.call == end.call))
                .map(|call| call.key.clone());
            if let Some(key) = key {
                if matches!(end.error, Some(crate::drawing::CallError::Store)) {
                    self.call_failed(&key, effects)
                        .expect("the associated drawing run is current");
                    continue;
                }
                let result = self
                    .current
                    .as_mut()
                    .and_then(|run| run.draw_calls.iter_mut().find(|call| call.call == end.call))
                    .and_then(|call| call.ended(end));
                if let Some((key, output)) = result {
                    self.drawn(&key, output, effects);
                }
            }
        }
        let holding = self.drawing.holding();
        if holding != self.ready_armed {
            self.ready_armed = holding;
            effects.push(if holding {
                Effect::ArmReadyWait
            } else {
                Effect::CancelReadyWait
            });
        }
    }

    fn forward_drawing_effect(&mut self, mut effect: Effect, effects: &mut Vec<Effect>) {
        let plain_say = self.narration == Some(Narration::Plain);
        if let Effect::Data(DataToShell::SceneStepCommit {
            run,
            scene,
            index,
            step,
            speech,
        }) = &mut effect
            && let Some(staged) = self
                .drawing_reply
                .take_if(|reply| reply.key.run == *run && reply.revision == *index + 1)
            && let Some(segment) = staged.segment
            && let Some(current) = self.current_attempt(staged.key)
            && let Some(reply) = &mut current.reply
        {
            let start = match step.start {
                Start::Speech if !plain_say => SegmentStart::Speech,
                Start::Speech | Start::Now | Start::After { .. } => SegmentStart::Plain,
            };
            let step = StepRef {
                scene: *scene,
                index: *index,
            };
            let mut out = Vec::new();
            *speech = reply.publish_say(segment, step, start, staged.say.as_str(), &mut out);
            effects.push(effect);
            speech::carry(current, out, effects);
            return;
        }
        effects.push(effect);
    }

    fn reserve_step_reply(&mut self, serial: u64, write: &Write, effects: &mut Vec<Effect>) {
        let Write::Step {
            run,
            thread,
            revision,
            step,
            ..
        } = write
        else {
            return;
        };
        if self.deleting_thread(*thread) {
            return;
        }
        let Some(say) = step
            .say
            .as_ref()
            .filter(|say| !say.as_str().trim().is_empty())
        else {
            return;
        };
        let owner = self
            .drawing
            .writing_call(serial)
            .expect("the step owns the writer");
        let Some((key, call)) = self
            .current
            .as_ref()
            .filter(|current| current.id == *run)
            .and_then(|current| current.draw_calls.iter().find(|call| &call.call == owner))
            .and_then(|call| {
                call.build.map(|build| {
                    (
                        build,
                        match call.key.route {
                            dispatch::Route::Model | dispatch::Route::ModelRound(_) => {
                                Some(call.key.call.clone())
                            }
                            dispatch::Route::Mcp(_) => None,
                        },
                    )
                })
            })
        else {
            return;
        };
        let Some(current) = self.current_attempt(key) else {
            return;
        };
        let segment = Self::reserve_say(current, say.as_str(), effects);
        self.drawing_reply = Some(StepReply {
            serial,
            key,
            revision: *revision,
            say: say.clone(),
            call,
            segment,
        });
    }

    pub(super) fn drawing_written(
        &mut self,
        serial: u64,
        reply: Result<Reply, ()>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let step = matches!(reply, Ok(Reply::Step(_)));
        let output = self.drawing.handle(Input::Written { serial, reply })?;
        if step
            && let Some(held) = self
                .drawing_reply
                .take_if(|held| held.serial == serial && held.segment.is_none())
            && let Some(run) = self.current_attempt(held.key)
        {
            let line = format!("\n\n{}\n\n", held.say.as_str());
            // shortcut: a crash before the reply settles loses this line with the unflushed text.
            run.unstored.narrate(&line, held.call.as_deref());
            Self::carry_reply_text(run, &line, effects);
        }
        self.drawing_output(output, effects);
        drop(self.drawing_reply.take_if(|held| held.serial == serial));
        Ok(())
    }

    pub(super) fn current_attempt(&mut self, key: BuildKey) -> Option<&mut super::Run> {
        self.current(key.run).ok().filter(|run| {
            run.build
                .as_ref()
                .is_some_and(|build| build.key() == key && build.admitted())
        })
    }

    pub(super) fn drawing_closed(&mut self, run: RunId, loss: Loss, effects: &mut Vec<Effect>) {
        if let Ok(output) = self.drawing.handle(Input::RunClosed { run, loss }) {
            self.drawing_output(output, effects);
        }
    }

    fn drawing_revoke(
        &mut self,
        scenes: Vec<SceneId>,
        successor: Option<SceneId>,
        effects: &mut Vec<Effect>,
    ) {
        self.drawing_held.retain_mut(|(_, frame, end)| {
            if scene_id(frame).is_some_and(|scene| scenes.contains(&scene)) {
                let Some(run) = end.take() else {
                    return false;
                };
                *frame = DataToShell::RunEnd { run };
            }
            true
        });
        let mut speech = Vec::new();
        if let Some(reply) = self.current.as_mut().and_then(|run| run.reply.as_mut()) {
            reply.scenes_removed(&scenes, &mut speech);
        }
        effects.push(Effect::RetireScenes { scenes, successor });
        if let Some(run) = &mut self.current {
            speech::carry(run, speech, effects);
        }
        let mut at = 0;
        while at < self.drawing_held.len() {
            if let DataToShell::RunEnd { run } = self.drawing_held[at].1 {
                let (thread, _, _) = self.drawing_held.remove(at);
                self.data_end(run, thread, effects);
            } else {
                at += 1;
            }
        }
    }

    pub(super) fn drawing_deleted(&mut self, thread: ThreadId, effects: &mut Vec<Effect>) {
        self.drawing_held.retain(|(owner, _, end)| {
            if *owner != thread {
                return true;
            }
            if let Some(run) = end {
                effects.push(Effect::Data(DataToShell::RunEnd { run: *run }));
            }
            false
        });
        if let Ok(output) = self.drawing.handle(Input::ThreadDeleted { thread }) {
            self.drawing_output(output, effects);
        }
    }

    pub(super) fn drawing_not_deleted(&mut self, thread: ThreadId, effects: &mut Vec<Effect>) {
        let held = std::mem::take(&mut self.drawing_held);
        for (owner, frame, end) in held {
            if owner == thread {
                effects.push(Effect::Data(frame));
                if let Some(run) = end {
                    effects.push(Effect::Data(DataToShell::RunEnd { run }));
                }
            } else {
                self.drawing_held.push((owner, frame, end));
            }
        }
    }

    pub(super) fn drawing_clear(&mut self, scenes: Vec<SceneId>, effects: &mut Vec<Effect>) {
        if let Ok(output) = self.drawing.handle(Input::Clear { scenes }) {
            self.drawing_output(output, effects);
        }
    }

    pub(super) fn drawing_settle(&mut self, effects: &mut Vec<Effect>) {
        let output = self.drawing.clears_pending(self.clearing.len());
        self.drawing_output(output, effects);
        let done = self
            .current
            .as_ref()
            .filter(|run| run.finishing && !run.outstanding() && !self.drawing.pending_for(run.id))
            .map(|run| run.id);
        if let Some(run) = done {
            self.round_end(run, effects)
                .expect("the finishing run is current");
        }
        let runs: Vec<_> = self.ending.iter().map(|run| run.id).collect();
        for run in runs {
            self.settle(run, effects);
        }
    }

    pub(super) fn drawing_retry(&mut self, effects: &mut Vec<Effect>) {
        let retried = self.drawing.retry();
        self.drawing_output(
            Output {
                effects: retried,
                calls: Vec::new(),
            },
            effects,
        );
    }
}

fn scene_id(message: &kumochi_protocol::message::DataToShell) -> Option<SceneId> {
    use kumochi_protocol::message::DataToShell;
    match message {
        DataToShell::SceneBegin { scene, .. }
        | DataToShell::SceneStepCommit { scene, .. }
        | DataToShell::SceneCutApplied { scene, .. }
        | DataToShell::SceneEnd { scene, .. } => Some(*scene),
        _ => None,
    }
}
