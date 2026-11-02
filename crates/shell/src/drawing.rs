//! The drawing as the shell shows it: at most four scenes of steps, and the recovery that
//! restores them on each connection. Plain state, no GPUI, so every rule is unit-tested.
//!
//! `Scenes` is authoritative: a shown scene it doesn't name goes. A named scene's snapshot
//! arrives in chunks and installs whole, only on the connection that described it and only if no
//! clear came first. Chunks that arrive before `Scenes` are held with their credit withheld, so
//! the brain's credit window bounds them. A live step for a new scene admits it the way the brain
//! does: the newest four by `SceneId`.

use std::collections::{BTreeMap, VecDeque};

use capsula_protocol::message::{
    ControlToBrain, MAX_STEP_LABEL, MAX_STEPS, MAX_VISIBLE_SCENES, RunId, SceneDescriptor, SceneId,
    Step,
};

/// The newest cleared ids remembered, so a clear the brain never got is sent again [guess].
const CLEARED_KEPT: usize = 64;

/// What the brain sent breaks the drawing's protocol, so the connection ends. It names sizes and
/// counts, never labels.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Broken {
    #[error("{0} scenes, more than the cap or one named twice")]
    Scenes(usize),
    #[error("a scene's step counts are out of range")]
    Counts,
    #[error("a step label of {0} bytes")]
    Label(usize),
    #[error("a snapshot past its {0} committed steps")]
    Overrun(u32),
}

/// What a change asks of the caller: a `ClearDrawing` to send, and credit to grant.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Effect {
    pub(crate) clear: Option<ControlToBrain>,
    pub(crate) credit: u32,
}

/// A `SceneSnapshot` frame, with its size on the pipe.
#[derive(Debug)]
pub(crate) struct Chunk {
    pub(crate) scene: SceneId,
    pub(crate) revision: u32,
    pub(crate) steps: Vec<Step>,
    pub(crate) last: bool,
    pub(crate) bytes: u32,
}

/// A scene on screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Scene {
    /// The run whose live steps drew it; none for a restored scene.
    run: Option<RunId>,
    /// Its steps, by index in the scene. A restored step shows as already activated.
    pub(crate) steps: Vec<(u32, Step)>,
}

/// A named scene whose snapshot is still arriving.
#[derive(Debug)]
struct Pending {
    revision: u32,
    committed: u32,
    steps: Vec<Step>,
}

#[derive(Debug, Default)]
enum Recovery {
    #[default]
    Offline,
    /// Connected, before `Scenes`: chunks are held in arrival order.
    Awaiting { held: Vec<Chunk> },
    /// After `Scenes`: the scenes still to install. Empty once recovery is done or cancelled.
    Receiving { pending: BTreeMap<SceneId, Pending> },
}

#[derive(Debug, Default)]
pub(crate) struct Drawing {
    /// At most [`MAX_VISIBLE_SCENES`], oldest first.
    shown: BTreeMap<SceneId, Scene>,
    /// The connection recovery runs on; chunks from any other are dropped and never credited.
    generation: u64,
    recovery: Recovery,
    /// Esc while awaiting `Scenes`: every id the next one names is cleared. It survives
    /// disconnects, so a recovery the user cancelled never shows.
    clear_intent: bool,
    /// The newest cleared ids, oldest first.
    cleared: VecDeque<SceneId>,
}

impl Drawing {
    /// A new connection: recovery awaits its `Scenes`.
    pub(crate) fn connected(&mut self, generation: u64) {
        self.generation = generation;
        self.recovery = Recovery::Awaiting { held: Vec::new() };
    }

    /// A new brain numbers its runs afresh, so no shown scene belongs to one of them.
    pub(crate) fn forget_runs(&mut self) {
        for scene in self.shown.values_mut() {
            scene.run = None;
        }
    }

    /// The connection ended. Shown scenes stay until the next `Scenes` reconciles them.
    pub(crate) fn disconnected(&mut self) {
        self.recovery = Recovery::Offline;
    }

    /// A scene is shown, or a recovery Esc could cancel is awaiting or receiving.
    pub(crate) fn active(&self) -> bool {
        !self.shown.is_empty()
            || match &self.recovery {
                Recovery::Offline => false,
                Recovery::Awaiting { .. } => !self.clear_intent,
                Recovery::Receiving { pending } => !pending.is_empty(),
            }
    }

    /// The shown scenes, oldest first.
    pub(crate) fn shown(&self) -> impl Iterator<Item = (&SceneId, &Scene)> {
        self.shown.iter()
    }

    /// The indices on screen of `run`'s steps, for its `Stop`.
    pub(crate) fn revealed(&self, run: RunId) -> Vec<u32> {
        self.shown
            .values()
            .filter(|scene| scene.run == Some(run))
            .flat_map(|scene| scene.steps.iter().map(|(index, _)| *index))
            .collect()
    }

    /// A live step the runs let through. A cleared scene's steps are dropped.
    pub(crate) fn step(&mut self, run: RunId, scene: SceneId, index: u32, step: Step) {
        if self.cleared.contains(&scene) || !self.admit(scene) {
            return;
        }
        let shown = self.shown.entry(scene).or_default();
        shown.run = Some(run);
        if shown.steps.len() < MAX_STEPS {
            shown.steps.push((index, step));
        }
    }

    /// Makes room for `scene` among the newest four, evicting the oldest. False when it's older
    /// than all four.
    fn admit(&mut self, scene: SceneId) -> bool {
        let pending = match &self.recovery {
            Recovery::Receiving { pending } => Some(pending),
            Recovery::Offline | Recovery::Awaiting { .. } => None,
        };
        if self.shown.contains_key(&scene) || pending.is_some_and(|p| p.contains_key(&scene)) {
            return true;
        }
        let recovering = pending.map_or(0, |pending| {
            let only = pending.keys().filter(|id| !self.shown.contains_key(id));
            only.count()
        });
        if self.shown.len() + recovering < MAX_VISIBLE_SCENES {
            return true;
        }
        let first_pending = pending.and_then(|pending| pending.keys().next());
        let oldest = *self
            .shown
            .keys()
            .next()
            .into_iter()
            .chain(first_pending)
            .min()
            .expect("four scenes are known");
        if scene < oldest {
            return false;
        }
        self.shown.remove(&oldest);
        if let Recovery::Receiving { pending } = &mut self.recovery {
            pending.remove(&oldest);
        }
        true
    }

    /// Esc clears the drawing at once, recovery included. While awaiting `Scenes`, the held
    /// chunks are dropped and the next `Scenes` clears every scene it names; otherwise the shown
    /// and pending scenes are cleared now, in one `ClearDrawing`.
    pub(crate) fn clear(&mut self) -> Effect {
        let mut ids: Vec<SceneId> = std::mem::take(&mut self.shown).into_keys().collect();
        let mut credit = 0;
        let awaiting = match &mut self.recovery {
            Recovery::Offline => false,
            Recovery::Awaiting { held } => {
                credit = held
                    .drain(..)
                    .fold(0, |sum: u32, chunk| sum.saturating_add(chunk.bytes));
                self.clear_intent = true;
                true
            }
            Recovery::Receiving { pending } => {
                ids.extend(std::mem::take(pending).into_keys());
                false
            }
        };
        ids.sort_unstable();
        ids.dedup();
        for id in &ids {
            self.remember(*id);
        }
        let clear = (!awaiting && !ids.is_empty())
            .then_some(ControlToBrain::ClearDrawing { scene_ids: ids });
        Effect { clear, credit }
    }

    /// The connection's `Scenes`: shown scenes it doesn't name go, the scenes it names start
    /// recovering unless they were cleared, and the held chunks are applied in arrival order.
    ///
    /// # Errors
    /// [`Broken`] when the list is over the cap, names a scene twice, or has counts out of range.
    pub(crate) fn apply_scenes(&mut self, scenes: &[SceneDescriptor]) -> Result<Effect, Broken> {
        let mut ids: Vec<SceneId> = scenes.iter().map(|named| named.scene).collect();
        ids.sort_unstable();
        ids.dedup();
        if scenes.len() > MAX_VISIBLE_SCENES || ids.len() < scenes.len() {
            return Err(Broken::Scenes(scenes.len()));
        }
        let in_range = |named: &SceneDescriptor| {
            named.committed <= named.revision
                && usize::try_from(named.revision).is_ok_and(|revision| revision <= MAX_STEPS)
        };
        if !scenes.iter().all(in_range) {
            return Err(Broken::Counts);
        }
        let held = match std::mem::take(&mut self.recovery) {
            Recovery::Awaiting { held } => held,
            Recovery::Offline | Recovery::Receiving { .. } => Vec::new(),
        };
        let mut suppressed = Vec::new();
        let mut pending = BTreeMap::new();
        for named in scenes {
            if self.clear_intent || self.cleared.contains(&named.scene) {
                suppressed.push(named.scene);
            } else {
                pending.insert(
                    named.scene,
                    Pending {
                        revision: named.revision,
                        committed: named.committed,
                        steps: Vec::new(),
                    },
                );
            }
        }
        self.clear_intent = false;
        self.shown.retain(|id, _| pending.contains_key(id));
        for id in &suppressed {
            self.remember(*id);
        }
        self.recovery = Recovery::Receiving { pending };
        let mut credit = 0u32;
        for chunk in held {
            credit = credit.saturating_add(self.chunk(chunk)?);
        }
        let clear = (!suppressed.is_empty()).then_some(ControlToBrain::ClearDrawing {
            scene_ids: suppressed,
        });
        Ok(Effect { clear, credit })
    }

    /// A `SceneSnapshot` from connection `generation`. Returns the credit to grant for it now:
    /// none while it's held for `Scenes`, and none for another connection's frame.
    ///
    /// # Errors
    /// [`Broken`] for a label over [`MAX_STEP_LABEL`] or more steps than the scene committed.
    pub(crate) fn apply_snapshot(&mut self, generation: u64, chunk: Chunk) -> Result<u32, Broken> {
        if generation != self.generation || matches!(self.recovery, Recovery::Offline) {
            return Ok(0);
        }
        self.chunk(chunk)
    }

    /// Classifies one chunk of the current connection, granting its credit exactly once.
    fn chunk(&mut self, chunk: Chunk) -> Result<u32, Broken> {
        if let Some(step) = chunk
            .steps
            .iter()
            .find(|step| step.label.len() > MAX_STEP_LABEL)
        {
            return Err(Broken::Label(step.label.len()));
        }
        let pending = match &mut self.recovery {
            Recovery::Awaiting { held } if !self.clear_intent => {
                held.push(chunk);
                return Ok(0);
            }
            Recovery::Receiving { pending } => pending,
            Recovery::Offline | Recovery::Awaiting { .. } => return Ok(chunk.bytes),
        };
        let Some(scene) = pending.get_mut(&chunk.scene) else {
            return Ok(chunk.bytes);
        };
        if chunk.revision != scene.revision {
            // Not the snapshot its descriptor named, so none of it installs.
            pending.remove(&chunk.scene);
            return Ok(chunk.bytes);
        }
        let committed = usize::try_from(scene.committed).unwrap_or(usize::MAX);
        if scene.steps.len() + chunk.steps.len() > committed {
            return Err(Broken::Overrun(scene.committed));
        }
        scene.steps.extend(chunk.steps);
        if !chunk.last {
            return Ok(chunk.bytes);
        }
        if let Some(done) = pending.remove(&chunk.scene)
            && done.steps.len() == committed
        {
            let steps = (0..).zip(done.steps).collect();
            self.shown.insert(chunk.scene, Scene { run: None, steps });
        }
        Ok(chunk.bytes)
    }

    fn remember(&mut self, id: SceneId) {
        if !self.cleared.contains(&id) {
            self.cleared.push_back(id);
        }
        if self.cleared.len() > CLEARED_KEPT {
            self.cleared.pop_front();
        }
    }
}

#[cfg(test)]
#[path = "drawing_tests.rs"]
mod tests;
