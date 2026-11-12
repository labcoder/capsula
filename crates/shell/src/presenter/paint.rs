//! The sampled frames the overlay paints: one reserved per adopted scene, kept beside the
//! presenter on the UI thread so each frame's charge stays inside the residency cap. Each frame
//! slot also owns its scene's paint cache, replaced only when the frame is.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::Rc,
};

use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_scene::{Frame, PreparedActivation, SceneView};

use super::{
    Effect, Presenter,
    scene::{Cut, Out, Scene},
};

/// A scene's retained paint state, staged from the view its frame samples. The presenter only
/// charges, moves and retires it; the painter that staged it uses it.
pub(crate) trait SceneCache: Any + Send {
    /// Requested bytes it owns, fixed when it was staged.
    fn bytes(&self) -> usize;
}

/// Stages a scene's paint cache from the view about to be committed. It never refuses: what it
/// can't stage fails when it paints.
pub(crate) trait Stage {
    fn stage(&self, view: &SceneView) -> Box<dyn SceneCache>;
    fn fixed_bytes(&self) -> usize;
}

pub(super) struct Slot {
    frame: Frame,
    /// Staged from the view this frame samples; it goes wherever the frame goes.
    cache: RefCell<Box<dyn SceneCache>>,
    /// Changes whenever the scene's view is replaced, so an older paint never counts.
    epoch: u64,
    /// The activated bound of the last sample at this epoch.
    sampled: Option<u32>,
    sampled_ms: Option<u64>,
    moving: bool,
    wake: Option<u64>,
    /// The bound a shown overlay last painted at this epoch.
    painted: Cell<Option<u32>>,
    /// The painted bound already acknowledged at this epoch.
    acknowledged: Option<u32>,
    /// A primitive failed to paint, so the scene paints no more and is cleared.
    failed: Cell<bool>,
    /// The activation this epoch installed hasn't begun: its first sample with something to
    /// paint is kept until a shown overlay paints it.
    pending: bool,
}

/// What one sample found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Some frame has an item to paint.
    pub(crate) visible: bool,
    /// A frame was adopted, replaced, resampled at a new bound or removed since the last sample.
    pub(crate) changed: bool,
}

/// A bound a shown overlay painted, at the epoch of the view it sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Painted {
    pub(crate) epoch: u64,
    pub(crate) bound: u32,
}

#[derive(Default)]
pub(crate) struct Frames {
    slots: [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES],
    epoch: u64,
    /// A slot left or changed placement since the last sample.
    invalidated: bool,
    /// Whether the overlay is shown, so only a paint that can reach the screen counts.
    pub(crate) shown: bool,
}

impl Frames {
    pub(crate) fn hide(&mut self) {
        self.shown = false;
        let epoch = self.next_epoch();
        for (_, slot) in self.slots.iter_mut().flatten() {
            slot.epoch = epoch;
            slot.painted.set(None);
            slot.acknowledged = None;
        }
    }

    pub(super) fn reservation(frame: &Frame) -> usize {
        frame.resident_bytes() + size_of::<RefCell<Self>>() + 2 * size_of::<usize>()
    }

    pub(super) fn adopt(
        &mut self,
        scene: SceneId,
        frame: Frame,
        cache: Box<dyn SceneCache>,
    ) -> usize {
        debug_assert!(
            self.slots.iter().flatten().all(|(id, _)| *id != scene),
            "each scene owns exactly one frame slot"
        );
        let bytes = Self::reservation(&frame);
        let epoch = self.next_epoch();
        let place = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("each adopted scene has one of the four reserved positions");
        *place = Some((
            scene,
            Slot {
                frame,
                cache: RefCell::new(cache),
                epoch,
                sampled: None,
                sampled_ms: None,
                moving: false,
                wake: None,
                painted: Cell::new(None),
                acknowledged: None,
                failed: Cell::new(false),
                pending: false,
            },
        ));
        self.slots
            .sort_unstable_by_key(|slot| slot.as_ref().map(|(id, _)| *id));
        bytes
    }

    /// The scene's view was replaced while the caller still holds the old one, so clearing the
    /// frame frees nothing here, and it binds to the new view, staged as `cache`, on its next
    /// sample. Returns the old view's cache for the caller to retire with it.
    pub(super) fn replaced(
        &mut self,
        scene: SceneId,
        cache: Box<dyn SceneCache>,
    ) -> Box<dyn SceneCache> {
        let slot = self
            .reset(scene)
            .expect("each adopted scene has a frame slot");
        slot.frame.clear();
        std::mem::replace(slot.cache.get_mut(), cache)
    }

    /// Installs a staged activation into the scene's view with its own frame, which the view
    /// clears while the outgoing objects still own the geometry; no earlier paint counts, and its
    /// first sample with something to paint is kept until it paints. Binds `cache`, staged from
    /// the activation's view, and returns the outgoing one.
    pub(super) fn activate(
        &mut self,
        scene: SceneId,
        view: &mut SceneView,
        activation: PreparedActivation,
        cache: Box<dyn SceneCache>,
    ) -> Box<dyn SceneCache> {
        let slot = self
            .reset(scene)
            .expect("each adopted scene has a frame slot");
        view.install(activation, &mut slot.frame)
            .expect("the presenter staged it from this view in the same update");
        // shortcut: a predecessor still waiting for its first paint keeps its staged clock; hold
        // the next activation until that paint if back-to-back steps show the gap.
        slot.pending = true;
        std::mem::replace(slot.cache.get_mut(), cache)
    }

    /// Starts a new epoch for the scene's frame, forgetting what was sampled and painted.
    fn reset(&mut self, scene: SceneId) -> Option<&mut Slot> {
        let epoch = self.next_epoch();
        let (_, slot) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == scene)?;
        slot.epoch = epoch;
        slot.sampled = None;
        slot.sampled_ms = None;
        slot.moving = false;
        slot.wake = None;
        slot.painted.set(None);
        slot.acknowledged = None;
        slot.pending = false;
        // `failed` stays set: a paint failure is terminal and paint_failed clears the scene.
        Some(slot)
    }

    /// Re-places the last sampled pose without beginning or dropping a pending entrance.
    pub(super) fn reposition(&mut self, scene: SceneId, view: &SceneView) {
        let (_, slot) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == scene)
            .expect("each adopted scene has a frame slot");
        if let Some(at) = slot.sampled_ms {
            view.frame(at, &mut slot.frame)
                .expect("placement preserves the view generation");
        }
        self.invalidated = true;
    }

    pub(super) fn take(&mut self, scene: SceneId) -> Option<Slot> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(id, _)| *id == scene))
            .and_then(Option::take)
            .map(|(_, slot)| slot);
        self.invalidated |= slot.is_some();
        slot
    }

    pub(super) fn take_all(&mut self) -> [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES] {
        self.invalidated |= self.slots.iter().any(Option::is_some);
        std::mem::take(&mut self.slots)
    }

    /// Samples each view into its scene's frame at `now_ms`, outside paint. A frame keeping its
    /// activation's first sample until it paints isn't sampled again, and one still empty after
    /// its first sample is sampled no later than its step's first appearance, which a late wake
    /// would otherwise pass.
    ///
    /// # Errors
    /// The first scene whose frame its view refuses as bound to another view; the caller
    /// releases the scene, so the frame goes to the dropper rather than being cleared here.
    pub(super) fn sample<'a>(
        &mut self,
        views: impl Iterator<Item = (SceneId, &'a SceneView, u64)>,
    ) -> Result<Sample, SceneId> {
        let mut sample = Sample {
            visible: false,
            changed: std::mem::take(&mut self.invalidated),
        };
        for (scene, view, now_ms) in views {
            let (_, slot) = self
                .slots
                .iter_mut()
                .flatten()
                .find(|(id, _)| *id == scene)
                .expect("each adopted scene has a frame slot");
            if slot.pending && !slot.frame.items().is_empty() {
                sample.visible = true;
                continue;
            }
            let now_ms = if slot.pending && slot.sampled_ms.is_some() {
                view.first_appearance()
                    .map_or(now_ms, |appears| appears.min(now_ms))
            } else {
                now_ms
            };
            if view.frame(now_ms, &mut slot.frame).is_err() {
                return Err(scene);
            }
            let bound = Some(view.activated());
            let moving = view.animating(now_ms);
            let advanced = slot.sampled_ms.is_some_and(|before| now_ms > before);
            sample.changed |= slot.sampled != bound
                || advanced
                    && (slot.moving || moving || slot.wake.is_some_and(|wake| wake <= now_ms));
            slot.sampled = bound;
            slot.sampled_ms = Some(now_ms);
            slot.moving = moving;
            slot.wake = view.next_wake(now_ms);
            sample.visible |= !slot.frame.items().is_empty();
        }
        Ok(sample)
    }

    /// Paints every frame in scene order, each with its cache; only the overlay's paint callback
    /// calls it. A shown overlay records each sampled bound it painted. A frame that fails paints
    /// no more and waits for [`Presenter::paint_failed`]. Whether a frame newly failed.
    pub(crate) fn paint<E>(
        &self,
        mut paint: impl FnMut(&Frame, &mut dyn SceneCache) -> Result<(), E>,
    ) -> bool {
        let shown = self.shown;
        let mut failed = false;
        for (_, slot) in self.slots.iter().flatten() {
            if slot.failed.get() {
                continue;
            }
            let result = paint(&slot.frame, &mut **slot.cache.borrow_mut());
            match result {
                Ok(()) if shown && !slot.frame.items().is_empty() => slot.painted.set(slot.sampled),
                Ok(()) => {}
                Err(_) => {
                    tracing::warn!("a drawing couldn't paint; clearing it");
                    slot.failed.set(true);
                    failed = true;
                }
            }
        }
        failed
    }

    /// The bound a shown overlay last painted for `scene`, at its current epoch.
    pub(crate) fn painted(&self, scene: SceneId) -> Option<Painted> {
        let (_, slot) = self.slots.iter().flatten().find(|(id, _)| *id == scene)?;
        if !self.shown || slot.failed.get() || slot.frame.items().is_empty() {
            return None;
        }
        Some(Painted {
            epoch: slot.epoch,
            bound: slot.painted.get()?,
        })
    }

    /// Takes a paint observation once: false for an epoch the scene's view has left, or a bound
    /// already acknowledged at this epoch.
    pub(crate) fn acknowledge(&mut self, scene: SceneId, painted: Painted) -> bool {
        if self.painted(scene) != Some(painted) {
            return false;
        }
        let Some((_, slot)) = self.slots.iter_mut().flatten().find(|(id, _)| *id == scene) else {
            return false;
        };
        if slot.epoch != painted.epoch
            || slot.painted.get() != Some(painted.bound)
            || slot.acknowledged == Some(painted.bound)
        {
            return false;
        }
        slot.acknowledged = Some(painted.bound);
        true
    }

    fn failed(&self) -> [Option<SceneId>; MAX_VISIBLE_SCENES] {
        self.slots.each_ref().map(|slot| {
            slot.as_ref()
                .filter(|(_, slot)| slot.failed.get())
                .map(|(id, _)| *id)
        })
    }

    fn next_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    pub(super) fn epoch(&self, scene: SceneId) -> Option<u64> {
        let (_, slot) = self.slots.iter().flatten().find(|(id, _)| *id == scene)?;
        Some(slot.epoch)
    }

    pub(super) fn sampled_ms(&self, scene: SceneId) -> Option<u64> {
        let (_, slot) = self.slots.iter().flatten().find(|(id, _)| *id == scene)?;
        slot.sampled_ms
    }

    /// Ends the scene's pending activation once a shown overlay has painted its kept sample,
    /// returning the scene time of that sample.
    pub(super) fn begin(&mut self, scene: SceneId) -> Option<u64> {
        let (_, slot) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == scene)?;
        if !slot.pending || slot.painted.get().is_none() {
            return None;
        }
        slot.pending = false;
        slot.sampled_ms
    }

    pub(crate) fn observations(&self) -> [Option<(SceneId, u64, u32)>; MAX_VISIBLE_SCENES] {
        self.slots.each_ref().map(|slot| {
            let (scene, slot) = slot.as_ref()?;
            let painted = self.painted(*scene)?;
            (slot.acknowledged != Some(painted.bound)).then_some((
                *scene,
                painted.epoch,
                painted.bound,
            ))
        })
    }

    #[cfg(test)]
    pub(super) fn ids(&self) -> [Option<SceneId>; MAX_VISIBLE_SCENES] {
        self.slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|(id, _)| *id))
    }
}

impl Presenter {
    /// The frames the overlay paints. Borrow them only inside its paint callback.
    pub(crate) fn frames(&self) -> Rc<RefCell<Frames>> {
        Rc::clone(&self.frames)
    }

    /// Whether the overlay is showing the scenes' frames, so Esc is held for them.
    pub(crate) fn drawn(&self) -> bool {
        self.frames.borrow().shown
    }

    /// Samples every scene's view at `now_ms`. A scene whose frame its view refuses is
    /// cleared, as an over-cap adoption is.
    #[cfg_attr(not(test), expect(dead_code, reason = "numeric sampler compatibility"))]
    pub(crate) fn sample(&mut self, now_ms: u64) -> (Sample, Vec<Effect>) {
        self.sample_times(|_| now_ms)
    }

    pub(crate) fn sample_at(&mut self, now: std::time::Instant) -> (Sample, Vec<Effect>) {
        self.sample_times(|scene| scene.activation.time(now))
    }

    fn sample_times(&mut self, time: impl Fn(&Scene) -> u64) -> (Sample, Vec<Effect>) {
        let mut out = Out::default();
        loop {
            let at = |scene: &Scene| {
                if scene.cut == Cut::Outstanding {
                    scene.stopped_ms.unwrap_or(0)
                } else {
                    time(scene)
                }
            };
            let views = self
                .scenes
                .iter()
                .map(|(id, scene)| (*id, &scene.view, at(scene)));
            let sampled = self.frames.borrow_mut().sample(views);
            match sampled {
                Ok(sample) => {
                    let frames = self.frames.borrow();
                    for (id, scene) in &mut self.scenes {
                        scene.sampled_ms = frames.sampled_ms(*id);
                    }
                    return (sample, out.finish());
                }
                Err(id) => {
                    self.release(id);
                    self.forget(id, &mut out);
                }
            }
        }
    }

    /// Clears every scene whose frame failed to paint.
    pub(crate) fn paint_failed(&mut self) -> Vec<Effect> {
        let mut out = Out::default();
        let failed = self.frames.borrow().failed();
        for id in failed.into_iter().flatten() {
            self.release(id);
            self.forget(id, &mut out);
        }
        out.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_slots_reuse_a_middle_release_and_take_all_once() {
        let mut frames = Frames::default();
        let cache = || Box::new(super::super::tests::Charged(0));
        let expected =
            Frame::new().resident_bytes() + size_of::<RefCell<Frames>>() + 2 * size_of::<usize>();
        for id in 1..=4 {
            assert_eq!(frames.adopt(SceneId(id), Frame::new(), cache()), expected);
        }
        assert_eq!(
            frames.ids(),
            [
                Some(SceneId(1)),
                Some(SceneId(2)),
                Some(SceneId(3)),
                Some(SceneId(4))
            ]
        );
        assert!(frames.take(SceneId(2)).is_some());
        assert!(frames.take(SceneId(2)).is_none());
        assert_eq!(frames.adopt(SceneId(5), Frame::new(), cache()), expected);
        assert_eq!(
            frames.ids(),
            [
                Some(SceneId(1)),
                Some(SceneId(3)),
                Some(SceneId(4)),
                Some(SceneId(5))
            ]
        );
        let all = frames.take_all();
        assert_eq!(
            all.each_ref().map(|slot| slot.as_ref().map(|(id, _)| *id)),
            [
                Some(SceneId(1)),
                Some(SceneId(3)),
                Some(SceneId(4)),
                Some(SceneId(5))
            ]
        );
        assert_eq!(frames.ids(), [None; 4]);
        assert!(frames.take_all().iter().all(Option::is_none));
    }
}
