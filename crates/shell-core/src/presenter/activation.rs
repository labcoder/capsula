//! Dormant synthetic activation state; paint records numbers and delivery stays outside paint.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{DataToShell, MAX_VISIBLE_SCENES, RunId, SceneFailure, SceneId};
use kumochi_scene::{SceneState, limits::MAX_STEPS, motion::synthetic_duration_ms, record::Start};

use super::{Broken, Cut, Effect, Out, Presenter, window::ConsumeError};

pub(super) struct Activation {
    origin: Instant,
    ends: [Option<u64>; MAX_STEPS],
    installed: Option<Installed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Installed {
    pub(super) generation: u64,
    pub(super) run: RunId,
    pub(super) index: u32,
    acknowledged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub generation: u64,
    pub run: RunId,
    pub scene: SceneId,
    pub index: u32,
}

impl Activation {
    pub(super) const BYTES: usize = size_of::<Self>();

    pub(super) fn new(state: &SceneState, now: Instant) -> Self {
        let mut ends = [None; MAX_STEPS];
        for (index, end) in ends.iter_mut().enumerate() {
            let index = u32::try_from(index).expect("64 lifetime slots fit u32");
            if index < state.activated() && state.step(index).is_some() {
                *end = Some(0);
            }
        }
        Self {
            origin: now,
            ends,
            installed: None,
        }
    }

    pub(super) fn time(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.origin).as_millis()).unwrap_or(u64::MAX)
    }

    pub(super) fn due(&self, state: &SceneState, index: u32) -> Result<u64, SceneFailure> {
        let step = state.step(index).ok_or(SceneFailure::Validation)?;
        let end = |index: u32| {
            usize::try_from(index)
                .ok()
                .and_then(|index| self.ends.get(index).copied().flatten())
                .ok_or(SceneFailure::Validation)
        };
        let previous = (0..index).rev().find(|index| state.step(*index).is_some());
        let previous_end = previous.map_or(Ok(0), end)?;
        match &step.start {
            Start::Now => Ok(0),
            Start::Speech => Ok(previous_end),
            Start::After { step: earlier, ms } => {
                let anchor = (0..index)
                    .find(|index| state.step(*index).is_some_and(|step| step.id == *earlier))
                    .ok_or(SceneFailure::Validation)?;
                Ok(end(anchor)?.saturating_add(u64::from(ms.get())))
            }
        }
    }

    pub(super) fn install(
        &mut self,
        generation: u64,
        run: RunId,
        index: u32,
        at: u64,
        duration: u64,
    ) {
        let slot = usize::try_from(index).expect("a validated lifetime index fits usize");
        self.ends[slot] = Some(at.saturating_add(duration));
        self.installed = Some(Installed {
            generation,
            run,
            index,
            acknowledged: false,
        });
    }

    /// Moves step `index`'s segment end with its entrance, which began `by` ms after staging.
    pub(super) fn delay(&mut self, index: u32, by: u64) {
        let slot = usize::try_from(index).expect("a validated lifetime index fits usize");
        if let Some(end) = &mut self.ends[slot] {
            *end = end.saturating_add(by);
        }
    }

    pub(super) fn halt(&mut self) {
        self.installed = None;
    }

    pub(super) fn retain(&mut self, state: &SceneState) {
        for (index, end) in self.ends.iter_mut().enumerate() {
            if state
                .step(u32::try_from(index).expect("64 slots fit u32"))
                .is_none()
            {
                *end = None;
            }
        }
    }
}

impl Presenter {
    pub fn advance(&mut self, generation: u64, now: Instant) -> Result<Vec<Effect>, Broken> {
        if self.generation != Some(generation) {
            return Ok(Vec::new());
        }
        let mut out = Out::default();
        let mut heads = [None; MAX_VISIBLE_SCENES];
        for (slot, (id, scene)) in heads.iter_mut().zip(&self.scenes) {
            if scene.cut == Cut::Open && !scene.skipping && scene.pending_consumption.is_none() {
                *slot = scene.ready.front().map(|head| (*id, head.step.index));
            }
        }
        for (id, index) in heads.into_iter().flatten() {
            self.activate_ready(generation, id, index, now, &mut out)?;
        }
        Ok(out.finish())
    }

    pub fn consumption_ready(
        &mut self,
        generation: u64,
        id: SceneId,
        index: u32,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        if self
            .scenes
            .get(&id)
            .is_some_and(|scene| scene.pending_consumption == Some(index))
        {
            self.activate_ready(generation, id, index, now, &mut out)?;
            let busy = self.lane.as_ref().is_some_and(|lane| lane.scene == id);
            if !busy
                && self.generation == Some(generation)
                && let Some(scene) = self.scenes.get_mut(&id)
                && scene.pending_consumption == Some(index)
            {
                scene.pending_consumption = None;
                self.drain(now, &mut out)?;
            }
        }
        Ok(out.finish())
    }

    fn activate_ready(
        &mut self,
        generation: u64,
        id: SceneId,
        index: u32,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        if self.generation != Some(generation) {
            return Ok(());
        }
        self.frames.borrow_mut().expire_retirement(now);
        if self.frames.borrow().guards_successor(id) {
            return Ok(());
        }
        let Some(scene) = self.scenes.get(&id) else {
            return Ok(());
        };
        let Some(head) = scene.ready.front().filter(|head| head.step.index == index) else {
            return Ok(());
        };
        let run = head.run;
        if scene.cut != Cut::Open || scene.skipping || self.fenced.is_some_and(|f| run <= f) {
            return Ok(());
        }
        let time = scene.activation.time(now);
        match scene.activation.due(scene.state.view(), index) {
            Ok(due) if due > time => return Ok(()),
            Ok(_) => {}
            Err(reason) => return self.fail(id, run, index, reason, now, out),
        }
        let segment =
            synthetic_duration_ms(scene.state.view().step(index).and_then(|s| s.say.as_ref()));
        match self.consume_ready(id, index, now, time, segment) {
            Ok(consumed) => {
                let super::window::Consumed {
                    step,
                    run,
                    epoch,
                    effects,
                } = consumed;
                drop(step);
                let effects = effects?;
                let current = self.generation == Some(generation)
                    && self.scenes.get(&id).is_some_and(|scene| {
                        scene.cut == Cut::Open
                            && !scene.skipping
                            && self.fenced.is_none_or(|f| run > f)
                            && scene.view.activated() == index.saturating_add(1)
                            && scene.activation.installed.is_some_and(|installed| {
                                installed.generation == generation
                                    && installed.run == run
                                    && installed.index == index
                            })
                    })
                    && self.frames.borrow().epoch(id) == Some(epoch);
                if !current
                    && let Some(scene) = self.scenes.get_mut(&id)
                    && scene.activation.installed.is_some_and(|installed| {
                        installed.generation == generation
                            && installed.run == run
                            && installed.index == index
                    })
                {
                    scene.activation.halt();
                }
                out.effects.extend(effects);
            }
            Err(ConsumeError::Busy) => {}
            Err(ConsumeError::Residency) => {
                self.fail(id, run, index, SceneFailure::Residency, now, out)?;
            }
            Err(_) => self.fail(id, run, index, SceneFailure::Validation, now, out)?,
        }
        Ok(())
    }

    /// Begins each open scene's activation whose kept first sample a shown overlay has painted,
    /// observed at `now`: what its step put starts that much later, so its entrance runs from
    /// the paint. Whether any began.
    pub fn begin_painted(&mut self, now: Instant) -> bool {
        let mut began = false;
        for (id, scene) in &mut self.scenes {
            let Some(kept) = self.frames.borrow_mut().begin(*id) else {
                continue;
            };
            if scene.cut == Cut::Open && !scene.skipping {
                let by = scene.activation.time(now).saturating_sub(kept);
                scene.view.delay_entrance(by);
                scene
                    .activation
                    .delay(scene.view.activated().saturating_sub(1), by);
                began = true;
            }
        }
        began
    }

    /// Takes only fixed numeric observations, outside paint; delivery belongs to the caller.
    pub fn take_shown(&mut self, generation: u64) -> [Option<Shown>; MAX_VISIBLE_SCENES] {
        let mut shown = [None; MAX_VISIBLE_SCENES];
        if self.generation != Some(generation) {
            return shown;
        }
        let mut frames = self.frames.borrow_mut();
        for (place, (id, scene)) in shown.iter_mut().zip(&mut self.scenes) {
            let Some(installed) = scene.activation.installed.as_mut() else {
                continue;
            };
            if scene.cut != Cut::Open
                || scene.skipping
                || installed.acknowledged
                || installed.generation != generation
                || self.fenced.is_some_and(|f| installed.run <= f)
            {
                continue;
            }
            let Some(painted) = frames.painted(*id) else {
                continue;
            };
            if painted.bound == installed.index.saturating_add(1)
                && frames.acknowledge(*id, painted)
            {
                installed.acknowledged = true;
                *place = Some(Shown {
                    generation,
                    run: installed.run,
                    scene: *id,
                    index: installed.index,
                });
            }
        }
        shown
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.scenes.values().any(|scene| {
            scene.cut == Cut::Open
                && !scene.skipping
                && scene.view.animating(scene.activation.time(now))
        })
    }

    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let frames = self.frames.borrow();
        self.scenes
            .iter()
            .filter(|(_, scene)| scene.cut == Cut::Open && !scene.skipping)
            .filter_map(|(id, scene)| {
                let time = scene.activation.time(now);
                let head = scene
                    .ready
                    .front()
                    .filter(|_| {
                        scene.pending_consumption.is_none() && !frames.guards_successor(*id)
                    })
                    .and_then(|head| {
                        scene
                            .activation
                            .due(scene.state.view(), head.step.index)
                            .ok()
                    });
                head.into_iter()
                    .chain(scene.view.next_wake(time))
                    .min()
                    .and_then(|at| {
                        scene
                            .activation
                            .origin
                            .checked_add(Duration::from_millis(at))
                    })
            })
            .min()
    }

    pub fn stoppable_run(&self) -> Option<RunId> {
        let scenes = self
            .scenes
            .values()
            .filter(|scene| scene.cut == Cut::Open && !scene.skipping)
            .flat_map(|scene| {
                scene
                    .activation
                    .installed
                    .filter(|i| Some(i.generation) == self.generation)
                    .map(|i| i.run)
                    .into_iter()
                    .chain(scene.running.as_ref().map(|r| r.run))
                    .chain(scene.ready.iter().map(|r| r.run))
                    .chain(scene.queue.iter().map(|t| t.run))
            });
        let held = self.held.iter().filter_map(|(frame, _)| match &**frame {
            DataToShell::SceneStepCommit { run, .. } => Some(*run),
            _ => None,
        });
        scenes
            .chain(self.lane.as_ref().map(|lane| lane.run))
            .chain(held)
            .chain(self.frames.borrow().retirement.as_ref().and_then(|r| r.run))
            .filter(|run| self.fenced.is_none_or(|f| *run > f))
            .max()
    }

    pub fn live_work(&self, now: Instant) -> bool {
        if self.frames.borrow().retirement.is_some() {
            return true;
        }
        let open_run = |run| self.fenced.is_none_or(|f| run > f);
        if self.lane.as_ref().is_some_and(|lane| open_run(lane.run))
            || self.held.iter().any(|(frame, _)| matches!(&**frame, DataToShell::SceneStepCommit { run, .. } if open_run(*run)))
        {
            return true;
        }
        self.scenes.values().any(|scene| {
            if scene.cut != Cut::Open || scene.skipping {
                return false;
            }
            scene.running.is_some()
                || !scene.ready.is_empty()
                || !scene.queue.is_empty()
                || scene.activation.installed.is_some_and(|installed| {
                    open_run(installed.run) && !scene.view.at_rest(scene.activation.time(now))
                })
        })
    }
}
