//! V1 drawing effects share the coordinator's run fences, store FIFO and clear debt.

use kumochi_protocol::message::{DataToShell, RunId, SceneId, StepRef, ThreadId};

use super::{Coordinator, Effect, Stale, StoreOp, speech};
use crate::drawing::{CallEnd, Drawing, Input, Output};

const MAX_RESULTS: usize = 64;

impl Coordinator {
    pub(crate) fn drawing_described(&mut self) {
        self.drawing_held.clear();
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

    /// Completed call outcomes for the caller, never diagnostic text.
    pub fn take_drawing_results(&mut self) -> Vec<CallEnd> {
        std::mem::take(&mut self.drawing_results)
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
            | Input::Record { thread, run, .. }
            | Input::CallFinished { thread, run, .. } => {
                let current = self.current(*run)?;
                if current.thread != *thread || !current.started() || current.finishing {
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
            let say = match &effect {
                Effect::Data(DataToShell::SceneStepCommit {
                    run,
                    scene,
                    index,
                    step,
                }) => step.say.as_ref().map(|say| {
                    let step = StepRef {
                        scene: *scene,
                        index: *index,
                    };
                    (*run, step, say.as_str().to_owned())
                }),
                _ => None,
            };
            effects.push(effect);
            if let Some((run, step, say)) = say {
                self.say(run, step, &say, effects);
            }
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
