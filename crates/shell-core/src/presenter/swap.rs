//! A bounded, presentation-only bridge after the brain has committed replacement.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{ControlToBrain, MAX_VISIBLE_SCENES, RunId, SceneId};
use kumochi_scene::{
    SceneView, budget::Usage, deferred::Deferred, display::PaintWork, layout::LayoutState,
    limits::MAX_RESIDENT_BYTES,
};

use super::{Activation, BOUND, Broken, Effect, Frames, Out, Presenter, paint::Slot, scene::Scene};

pub(super) struct FrozenPicture {
    pub(super) id: SceneId,
    pub(super) slot: Slot,
    _geometry: (LayoutState, SceneView),
    usage: Usage,
    bytes: usize,
    work: PaintWork,
}

pub(super) struct Retirement {
    pub(super) successor: SceneId,
    pub(super) run: Option<RunId>,
    pub(super) expires: Instant,
    pub(super) epoch: Option<u64>,
    pub(super) pictures: [Option<FrozenPicture>; MAX_VISIBLE_SCENES],
}

impl std::fmt::Debug for FrozenPicture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrozenPicture { .. }")
    }
}

impl std::fmt::Debug for Retirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retirement")
            .field("pictures", &self.pictures.iter().flatten().count())
            .finish_non_exhaustive()
    }
}

impl Retirement {
    pub(super) fn bytes(&self) -> usize {
        self.pictures.iter().flatten().map(|p| p.bytes).sum()
    }

    pub(super) fn work(&self) -> PaintWork {
        self.pictures
            .iter()
            .flatten()
            .fold(PaintWork::default(), |sum, p| sum + p.work)
    }
}

impl Frames {
    pub(super) fn successor(&self) -> Option<SceneId> {
        self.retirement.as_ref().map(|r| r.successor)
    }

    pub(super) fn discard_retirement(&mut self) {
        if let Some(retirement) = self.retirement.take() {
            let bytes = retirement.bytes();
            if retirement.pictures.iter().any(Option::is_some) {
                self.invalidated = true;
                drop(Deferred::new(retirement.pictures, bytes));
            }
        }
    }

    pub(super) fn expire_retirement(&mut self, now: Instant) {
        if self.retirement.as_ref().is_some_and(|r| now >= r.expires) {
            self.discard_retirement();
        }
    }

    pub(super) fn retirement_bytes(&self) -> usize {
        self.retirement.as_ref().map_or(0, Retirement::bytes)
    }

    pub(super) fn retirement_work(&self) -> PaintWork {
        self.retirement
            .as_ref()
            .map_or_else(PaintWork::default, Retirement::work)
    }

    pub(super) fn bind_successor(&mut self, id: SceneId, run: RunId) {
        if let Some(retirement) = &mut self.retirement
            && retirement.successor == id
            && retirement.run.is_none()
        {
            retirement.run = Some(run);
        }
    }

    pub(super) fn guards_successor(&self, id: SceneId) -> bool {
        self.retirement.as_ref().is_some_and(|r| {
            r.successor == id
                && r.epoch.is_some_and(|epoch| self.epoch(id) == Some(epoch))
                && self.painted(id).is_none()
        })
    }

    pub(super) fn handoff(&mut self) {
        let Some(retirement) = &self.retirement else {
            return;
        };
        let successor = retirement.successor;
        if retirement.epoch.is_some() {
            if self.painted(successor).is_some() {
                self.discard_retirement();
            }
            return;
        }
        let Some(epoch) = self.slots.iter().flatten().find_map(|(id, slot)| {
            (*id == successor
                && slot.sampled.is_some()
                && !slot.failed.get()
                && !slot.frame.items().is_empty())
            .then_some(slot.epoch)
        }) else {
            return;
        };
        let retirement = self.retirement.as_mut().expect("the attempt was just read");
        retirement.epoch = Some(epoch);
        if retirement.pictures.iter().any(Option::is_some) {
            let bytes = retirement.bytes();
            let pictures = std::mem::take(&mut retirement.pictures);
            drop(Deferred::new(pictures, bytes));
        }
        self.invalidated = true;
    }

    pub(super) fn retire_failed_pictures(&mut self) {
        if let Some(retirement) = &mut self.retirement {
            for picture in &mut retirement.pictures {
                if picture.as_ref().is_some_and(|p| p.slot.failed.get()) {
                    let picture = picture.take().expect("the failed picture was just read");
                    let bytes = picture.bytes;
                    drop(Deferred::new(picture, bytes));
                    self.invalidated = true;
                }
            }
        }
    }
}

impl Presenter {
    pub(super) fn replaced(
        &mut self,
        scenes: &[SceneId],
        successor: SceneId,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        if scenes.is_empty()
            || scenes.len() > MAX_VISIBLE_SCENES
            || scenes.contains(&successor)
            || scenes
                .iter()
                .enumerate()
                .any(|(at, id)| scenes[..at].contains(id))
            || self.scenes.contains_key(&successor)
        {
            return Err(Broken::Scenes);
        }
        self.frames.borrow_mut().discard_retirement();
        let effect = self.assembler.remove(scenes);
        let mut retirement = Retirement {
            successor,
            run: None,
            expires: now + Duration::from_millis(250),
            epoch: None,
            pictures: std::array::from_fn(|_| None),
        };
        // The shown screen scene's run keeps its watch for a successor that is its screen scene.
        if self.watch.replaced(scenes, successor) {
            super::screen::left();
        }
        for (place, id) in retirement.pictures.iter_mut().zip(scenes) {
            self.remember(*id);
            let Some(mut scene) = self.scenes.remove(id) else {
                continue;
            };
            scene.halt();
            let bytes = scene.held().saturating_sub(Activation::BYTES);
            let mut usage = scene.state.view().usage();
            usage.scenes = 0;
            let work = scene.paint;
            let screen = scene.screen;
            let Scene {
                state, rest, view, ..
            } = scene;
            state.release(&mut self.global).expect(BOUND);
            let slot = self.frames.borrow_mut().take(*id);
            let sampled = slot.as_ref().is_some_and(|slot| {
                slot.sampled.is_some() && !slot.failed.get() && !slot.frame.items().is_empty()
            });
            // A screen scene's marks go with it: no picture of them outlives the scene.
            if sampled && !screen {
                *place = Some(FrozenPicture {
                    id: *id,
                    slot: slot.expect("the sampled slot was just read"),
                    _geometry: (rest, view),
                    usage,
                    bytes,
                    work,
                });
            } else {
                drop(Deferred::new((rest, view, slot), bytes));
            }
        }
        self.frames.borrow_mut().retirement = Some(retirement);
        self.make_room(self.residency(), self.paint_work());
        out.credit(effect.credit);
        match effect.batch {
            Some(batch) if matches!(self.phase, super::Phase::Assembling) => {
                self.recover(batch, now, out)
            }
            _ => Ok(()),
        }
    }

    pub(super) fn make_room(&mut self, bytes: usize, work: PaintWork) {
        let fits = {
            let frames = self.frames.borrow();
            frames.retirement.as_ref().is_none_or(|r| {
                let charges = r
                    .pictures
                    .each_ref()
                    .map(|p| p.as_ref().map_or_else(Usage::default, |p| p.usage));
                let pictures = r.pictures.iter().flatten().count();
                let displayed = self.scenes.len()
                    - usize::from(self.scenes.contains_key(&r.successor))
                    + pictures;
                bytes <= MAX_RESIDENT_BYTES
                    && work.within_limits()
                    && displayed <= MAX_VISIBLE_SCENES
                    && self.global.check_retained(&charges).is_ok()
            })
        };
        if !fits {
            self.frames.borrow_mut().discard_retirement();
        }
    }

    pub fn discard_replacement(&mut self) {
        self.frames.borrow_mut().discard_retirement();
    }

    /// A refused host clears current scenes as failures, not as an explicit user clear.
    pub fn refuse_all(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        for effect in self.clear(now)? {
            if let Effect::Control(ControlToBrain::ClearDrawing { scene_ids }) = effect {
                out.effects.extend(
                    scene_ids
                        .into_iter()
                        .map(|scene| Effect::Control(ControlToBrain::SceneFailed { scene })),
                );
            } else {
                out.effects.push(effect);
            }
        }
        Ok(out.finish())
    }
}
