//! V1 drawing effects share the coordinator's run fences, store FIFO and clear debt.

use kumochi_protocol::message::{RunId, SceneId, ThreadId};

use super::{Coordinator, Effect, Stale, StoreOp};
use crate::drawing::{CallEnd, Drawing, Input, Output};

const MAX_RESULTS: usize = 64;

impl Coordinator {
    pub(crate) fn drawing_described(&mut self) {
        self.drawing_held.clear();
    }

    pub(super) fn drawing_written(
        &mut self,
        serial: u64,
        reply: Result<crate::drawing::Reply, ()>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        self.drawing_input(Input::Written { serial, reply }, effects)?;
        self.drawing_settle(effects);
        Ok(())
    }

    pub(super) fn stop_drawings(
        &mut self,
        run: RunId,
        cuts: kumochi_protocol::message::SceneCuts,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if self.current_run() == Some(run) {
            let current = self.take(run)?;
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

    /// Restores the complete visible v1 batch before any run is accepted.
    /// # Errors
    /// Rejects invalid stored histories or final global usage.
    pub fn restore_drawings(
        &mut self,
        scenes: Vec<crate::drawing::StoredScene>,
    ) -> Result<(), kumochi_scene::StepError> {
        self.drawing = Drawing::recover(scenes)?;
        Ok(())
    }

    /// Completed call outcomes for the caller, never diagnostic text.
    pub fn take_drawing_results(&mut self) -> Vec<CallEnd> {
        std::mem::take(&mut self.drawing_results)
    }

    /// A frozen, validated v1 snapshot for the next connection's data FIFO.
    /// # Errors
    /// Rejects an inconsistent producer-built descriptor or chunk.
    pub fn drawing_snapshot(
        &self,
    ) -> Result<crate::drawing::Snapshot, kumochi_protocol::message::SceneDataError> {
        if !self.drawing.settled() || !self.clearing.is_empty() || !self.keeping.is_empty() {
            return Err(kumochi_protocol::message::SceneDataError::Descriptor);
        }
        self.drawing.snapshot()
    }

    pub(super) fn drawing_input(
        &mut self,
        input: Input,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        match &input {
            Input::Call { thread, run, .. }
            | Input::Record { thread, run, .. }
            | Input::CallFinished { thread, run, .. } => {
                let current = self.current(*run)?;
                if current.thread != *thread || !current.turn_stored || current.finishing {
                    return Err(Stale);
                }
            }
            Input::StepShown { run, .. } | Input::StepFailed { run, .. }
                if *run < self.session_start =>
            {
                return Err(Stale);
            }
            _ => {}
        }
        let output = self.drawing.handle(input)?;
        self.drawing_output(output, effects);
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
                    self.drawing_held.push((thread, message));
                }
                continue;
            }
            if let Effect::RevokeScenes { scenes } = &effect {
                self.drawing_held.retain(|(_, frame)| {
                    scene_id(frame).is_none_or(|scene| !scenes.contains(&scene))
                });
            }
            if let Effect::Store(StoreOp::ClearScenes { scenes }) = &effect {
                for scene in scenes {
                    if !self.clearing.iter().any(|(owed, _)| owed == scene) {
                        self.clearing.push((*scene, true));
                    }
                }
            }
            effects.push(effect);
        }
        if self.drawing_results.len() + output.calls.len() > MAX_RESULTS {
            effects.push(Effect::Broken("drawing call outcomes were not consumed"));
        } else {
            self.drawing_results.extend(output.calls);
        }
    }

    pub(super) fn drawing_closed(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        if let Ok(output) = self.drawing.handle(Input::RunClosed { run }) {
            self.drawing_output(output, effects);
        }
    }

    pub(super) fn drawing_deleted(&mut self, thread: ThreadId, effects: &mut Vec<Effect>) {
        self.drawing_held.retain(|(owner, _)| *owner != thread);
        if let Ok(output) = self.drawing.handle(Input::ThreadDeleted { thread }) {
            self.drawing_output(output, effects);
        }
    }

    pub(super) fn drawing_not_deleted(&mut self, thread: ThreadId, effects: &mut Vec<Effect>) {
        let held = std::mem::take(&mut self.drawing_held);
        for (owner, frame) in held {
            if owner == thread {
                effects.push(Effect::Data(frame));
            } else {
                self.drawing_held.push((owner, frame));
            }
        }
    }

    pub(super) fn drawing_clear(&mut self, scenes: Vec<SceneId>, effects: &mut Vec<Effect>) {
        if let Ok(output) = self.drawing.handle(Input::Clear { scenes }) {
            self.drawing_output(output, effects);
        }
    }

    pub(super) fn drawing_settle(&mut self, effects: &mut Vec<Effect>) {
        let done = self
            .current
            .as_ref()
            .filter(|run| run.finishing && !run.outstanding() && !self.drawing.pending_for(run.id))
            .map(|run| run.id);
        if let Some(run) = done
            && let Ok(done) = self.take(run)
        {
            self.end(
                done,
                crate::store::RunEnd::Done,
                None,
                super::Notify::Shell,
                effects,
            );
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
