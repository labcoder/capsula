//! The visible set in the core loop: which scenes are visible, their kept steps, and the clears
//! not yet durable. Every way a scene leaves the set writes a `scene_cleared`, so the store's
//! recovery at start reads the set this holds.

use capsula_protocol::message::{MAX_VISIBLE_SCENES, RunId, SceneId, Step};

use super::{Coordinator, Effect, Stale, StoreOp};
use crate::store::SceneState;

impl Coordinator {
    /// The visible set, oldest first.
    pub fn visible(&self) -> &[SceneState] {
        &self.scenes
    }

    /// Nothing can still move the visible set: no ended run waits on the store, so every first
    /// step and fifth-scene write is answered, and every clear is durable.
    pub fn settled(&self) -> bool {
        self.ending.is_empty() && self.clearing.is_empty() && self.keeping.is_empty()
    }

    /// A clear's or a kept selection's write is in the store.
    pub fn clears_in_flight(&self) -> bool {
        self.clearing.iter().any(|(_, writing)| *writing)
            || self.keeping.iter().any(|(_, _, writing)| *writing)
    }

    /// Writes again every clear whose write failed.
    pub fn retry_clears(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.retry(&mut effects);
        effects
    }

    /// The scene a run's first step evicts: the oldest, when four are visible. Only one first
    /// step is ever in the store, because a run's engine starts only once every older run's
    /// outcome is queued, which waits for all of that run's writes.
    pub(super) fn evict(&self) -> Option<SceneId> {
        (self.scenes.len() >= MAX_VISIBLE_SCENES).then(|| self.scenes[0].scene)
    }

    /// A run's first step committed: its scene joins the set, and the scene its write evicted
    /// leaves it, as the same transaction recorded.
    pub(super) fn begin_scene(
        &mut self,
        scene: SceneId,
        (thread, run): (capsula_protocol::message::ThreadId, RunId),
        evict: Option<SceneId>,
    ) {
        self.scenes.retain(|state| Some(state.scene) != evict);
        let at = self.scenes.partition_point(|state| state.scene < scene);
        self.scenes.insert(
            at,
            SceneState {
                scene,
                thread,
                run,
                revision: 0,
                kept: Vec::new(),
                stopped: false,
            },
        );
        debug_assert!(
            self.scenes.len() <= MAX_VISIBLE_SCENES,
            "evictions keep the set at four"
        );
    }

    /// A step committed: it counts in the revision, and `kept` is the step unless its run was
    /// stopped.
    pub(super) fn scene_stored(&mut self, scene: SceneId, kept: Option<&Step>) {
        if let Some(state) = self.scenes.iter_mut().find(|state| state.scene == scene) {
            state.revision = state.revision.saturating_add(1);
            state.kept.extend(kept.cloned());
        }
    }

    /// The shell cleared `scenes`: they leave the set at once, so no later `Scenes` lists them,
    /// and wait in `clearing` until their write commits. An id that isn't visible, because it's
    /// unknown, already durable or already clearing, changes nothing.
    pub(super) fn clear(&mut self, scenes: &[SceneId], effects: &mut Vec<Effect>) {
        let mut cleared = Vec::new();
        for scene in scenes {
            if let Some(at) = self.scenes.iter().position(|state| state.scene == *scene) {
                self.scenes.remove(at);
                self.clearing.push((*scene, true));
                cleared.push(*scene);
            }
        }
        if !cleared.is_empty() {
            effects.push(Effect::Store(StoreOp::ClearScenes { scenes: cleared }));
        }
    }

    /// A clear's write committed: those scenes' clears are durable.
    pub(super) fn cleared(&mut self, scenes: &[SceneId]) {
        self.clearing.retain(|(scene, _)| !scenes.contains(scene));
    }

    /// A clear's write failed: those scenes wait for a retry.
    pub(super) fn clear_failed(&mut self, scenes: &[SceneId]) {
        for (scene, writing) in &mut self.clearing {
            if scenes.contains(scene) {
                *writing = false;
            }
        }
    }

    /// A new connection is served: only runs it starts from here are its own to stop.
    pub fn begin_session(&mut self) {
        self.session_start = RunId(self.last_run.0 + 1);
    }

    /// A Stop for a run that already ended, which the shell could still stop while it drew the
    /// run's last steps. Its scene keeps what stayed, as a current run's Stop would: with its
    /// outcome when its writes are still pending, else in a write of its own. A run already
    /// stopped, or one an earlier connection started, is stale.
    pub(super) fn late_stop(
        &mut self,
        run: RunId,
        mut revealed: Vec<u32>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        if run < self.session_start {
            return Err(Stale);
        }
        if let Some(ended) = self.ending.iter_mut().find(|ended| ended.id == run) {
            let (outstanding, steps) = (ended.outstanding(), ended.steps);
            let ending = ended.ending.as_mut().expect("an ending run has an Ending");
            if ending.revealed.is_some() {
                return Err(Stale);
            }
            if outstanding {
                // Its outcome isn't written yet, so the selection goes with it once its writes
                // settle, the way a Stop's does.
                revealed.retain(|&index| index < steps);
                revealed.sort_unstable();
                revealed.dedup();
                ending.revealed = Some(revealed);
                return Ok(());
            }
        }
        let state = self
            .scenes
            .iter()
            .find(|state| state.run == run && !state.stopped)
            .ok_or(Stale)?;
        let scene = state.scene;
        if self.keeping.iter().any(|(kept, _, _)| *kept == scene) {
            return Err(Stale);
        }
        revealed.retain(|&index| index < state.revision);
        revealed.sort_unstable();
        revealed.dedup();
        if let Some(cleared) = keep(&mut self.scenes, scene, &revealed) {
            self.clearing.push((cleared, true));
            let scenes = vec![cleared];
            effects.push(Effect::Store(StoreOp::ClearScenes { scenes }));
        } else {
            self.keeping.push((scene, revealed.clone(), true));
            effects.push(Effect::Store(StoreOp::KeepScene { scene, revealed }));
        }
        Ok(())
    }

    /// A kept selection's write committed.
    pub(super) fn kept(&mut self, scene: SceneId) {
        self.keeping.retain(|(kept, _, _)| *kept != scene);
    }

    /// A kept selection's write failed: it waits for a retry.
    pub(super) fn keep_failed(&mut self, scene: SceneId) {
        if let Some(entry) = self.keeping.iter_mut().find(|(kept, _, _)| *kept == scene) {
            entry.2 = false;
        }
    }

    /// A run left without an outcome on record: its scene's clear, or else the selection its
    /// Stop kept, is still owed, so it's retried on its own rather than forgotten.
    pub(super) fn owe(&mut self, clear: Option<SceneId>, kept: Option<(SceneId, Vec<u32>)>) {
        if let Some(scene) = clear {
            self.clearing.push((scene, false));
        } else if let Some((scene, revealed)) = kept {
            self.keeping.push((scene, revealed, false));
        }
    }

    /// Quit: one write clears the visible set and every clear not yet durable. A kept selection
    /// not yet written goes too: its scene is cleared.
    pub(super) fn clear_visible(&mut self, effects: &mut Vec<Effect>) {
        self.keeping.retain(|(_, _, writing)| *writing);
        let visible: Vec<SceneId> = self.scenes.drain(..).map(|state| state.scene).collect();
        self.clearing
            .extend(visible.into_iter().map(|scene| (scene, false)));
        self.retry(effects);
    }

    /// Retries failed clears with any store operation already going out.
    pub(super) fn retry_with(&mut self, effects: &mut Vec<Effect>) {
        if effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(_)))
        {
            self.retry(effects);
        }
    }

    fn retry(&mut self, effects: &mut Vec<Effect>) {
        // A selection for a scene that has since left the set is moot: it's cleared.
        let scenes = &self.scenes;
        self.keeping.retain(|(scene, _, writing)| {
            *writing || scenes.iter().any(|state| state.scene == *scene)
        });
        for (scene, revealed, writing) in &mut self.keeping {
            if !*writing {
                *writing = true;
                effects.push(Effect::Store(StoreOp::KeepScene {
                    scene: *scene,
                    revealed: revealed.clone(),
                }));
            }
        }
        let mut scenes = Vec::new();
        for (scene, writing) in &mut self.clearing {
            if !*writing {
                *writing = true;
                scenes.push(*scene);
            }
        }
        if !scenes.is_empty() {
            effects.push(Effect::Store(StoreOp::ClearScenes { scenes }));
        }
    }
}

/// A stopped run's scene keeps only the steps that stayed on screen. Returns the scene when it
/// keeps none, after taking it out of the set; the run's outcome then clears it.
pub(super) fn keep(
    scenes: &mut Vec<SceneState>,
    scene: SceneId,
    revealed: &[u32],
) -> Option<SceneId> {
    let at = scenes.iter().position(|state| state.scene == scene)?;
    let state = &mut scenes[at];
    let kept = std::mem::take(&mut state.kept);
    state.kept = (0_u32..)
        .zip(kept)
        .filter(|(index, _)| revealed.contains(index))
        .map(|(_, step)| step)
        .collect();
    state.stopped = true;
    if !state.kept.is_empty() {
        return None;
    }
    scenes.remove(at);
    Some(scene)
}
