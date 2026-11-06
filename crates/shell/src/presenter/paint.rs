//! The sampled frames the overlay paints: one reserved per adopted scene, kept beside the
//! presenter on the UI thread so each frame's charge stays inside the residency cap.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_scene::{Frame, PreparedActivation, SceneView};

use super::{Effect, Presenter, scene::Out};

pub(super) struct Slot {
    frame: Frame,
    /// Changes whenever the scene's view is replaced, so an older paint never counts.
    epoch: u64,
    /// The activated bound of the last sample at this epoch.
    sampled: Option<u32>,
    /// The bound a shown overlay last painted at this epoch.
    painted: Cell<Option<u32>>,
    /// The painted bound already acknowledged at this epoch.
    acknowledged: Option<u32>,
    /// A primitive failed to paint, so the scene paints no more and is cleared.
    failed: Cell<bool>,
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6d's StepShown consumes this observation")
)]
pub(crate) struct Painted {
    pub(crate) epoch: u64,
    pub(crate) bound: u32,
}

#[derive(Default)]
pub(crate) struct Frames {
    slots: [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES],
    epoch: u64,
    /// A slot left since the last sample.
    removed: bool,
    /// Whether the overlay is shown, so only a paint that can reach the screen counts.
    pub(crate) shown: bool,
}

impl Frames {
    pub(super) fn reservation(frame: &Frame) -> usize {
        frame.resident_bytes() + size_of::<RefCell<Self>>() + 2 * size_of::<usize>()
    }

    pub(super) fn adopt(&mut self, scene: SceneId, frame: Frame) -> usize {
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
                epoch,
                sampled: None,
                painted: Cell::new(None),
                acknowledged: None,
                failed: Cell::new(false),
            },
        ));
        self.slots
            .sort_unstable_by_key(|slot| slot.as_ref().map(|(id, _)| *id));
        bytes
    }

    /// The scene's view was replaced while the caller still holds the old one, so clearing the
    /// frame frees nothing here, and it binds to the new view on its next sample.
    pub(super) fn replaced(&mut self, scene: SceneId) {
        if let Some(frame) = self.reset(scene) {
            frame.clear();
        }
    }

    /// Installs a staged activation into the scene's view with its own frame, which the view
    /// clears while the outgoing objects still own the geometry; no earlier paint counts.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "B6d's activation owner is the production caller")
    )]
    pub(super) fn activate(
        &mut self,
        scene: SceneId,
        view: &mut SceneView,
        activation: PreparedActivation,
    ) {
        let frame = self
            .reset(scene)
            .expect("each adopted scene has a frame slot");
        view.install(activation, frame)
            .expect("the presenter staged it from this view in the same update");
    }

    /// Starts a new epoch for the scene's frame, forgetting what was sampled and painted.
    fn reset(&mut self, scene: SceneId) -> Option<&mut Frame> {
        let epoch = self.next_epoch();
        let (_, slot) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|(id, _)| *id == scene)?;
        slot.epoch = epoch;
        slot.sampled = None;
        slot.painted.set(None);
        slot.acknowledged = None;
        // `failed` stays set: a paint failure is terminal and paint_failed clears the scene.
        Some(&mut slot.frame)
    }

    pub(super) fn take(&mut self, scene: SceneId) -> Option<Slot> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(id, _)| *id == scene))
            .and_then(Option::take)
            .map(|(_, slot)| slot);
        self.removed |= slot.is_some();
        slot
    }

    pub(super) fn take_all(&mut self) -> [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES] {
        self.removed |= self.slots.iter().any(Option::is_some);
        std::mem::take(&mut self.slots)
    }

    /// Samples each view into its scene's frame at `now_ms`, outside paint.
    ///
    /// # Errors
    /// The first scene whose frame its view refuses as bound to another view; the caller
    /// releases the scene, so the frame goes to the dropper rather than being cleared here.
    pub(super) fn sample<'a>(
        &mut self,
        views: impl Iterator<Item = (SceneId, &'a SceneView)>,
        now_ms: u64,
    ) -> Result<Sample, SceneId> {
        let mut sample = Sample {
            visible: false,
            changed: std::mem::take(&mut self.removed),
        };
        for (scene, view) in views {
            let (_, slot) = self
                .slots
                .iter_mut()
                .flatten()
                .find(|(id, _)| *id == scene)
                .expect("each adopted scene has a frame slot");
            if view.frame(now_ms, &mut slot.frame).is_err() {
                return Err(scene);
            }
            let bound = Some(view.activated());
            sample.changed |= slot.sampled != bound;
            slot.sampled = bound;
            sample.visible |= !slot.frame.items().is_empty();
        }
        Ok(sample)
    }

    /// Paints every frame in scene order; only the overlay's paint callback calls it. A shown
    /// overlay records each sampled bound it painted. A frame that fails paints no more and
    /// waits for [`Presenter::paint_failed`]. Whether a frame newly failed.
    pub(crate) fn paint<E>(&self, mut paint: impl FnMut(&Frame) -> Result<(), E>) -> bool {
        let shown = self.shown;
        let mut failed = false;
        for (_, slot) in self.slots.iter().flatten() {
            if slot.failed.get() {
                continue;
            }
            match paint(&slot.frame) {
                Ok(()) if shown => slot.painted.set(slot.sampled),
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "B6d's StepShown consumes this observation")
    )]
    pub(crate) fn painted(&self, scene: SceneId) -> Option<Painted> {
        let (_, slot) = self.slots.iter().flatten().find(|(id, _)| *id == scene)?;
        Some(Painted {
            epoch: slot.epoch,
            bound: slot.painted.get()?,
        })
    }

    /// Takes a paint observation once: false for an epoch the scene's view has left, or a bound
    /// already acknowledged at this epoch.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "B6d's StepShown consumes this observation")
    )]
    pub(crate) fn acknowledge(&mut self, scene: SceneId, painted: Painted) -> bool {
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

    #[cfg(test)]
    pub(super) fn epoch(&self, scene: SceneId) -> Option<u64> {
        let (_, slot) = self.slots.iter().flatten().find(|(id, _)| *id == scene)?;
        Some(slot.epoch)
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
    pub(crate) fn sample(&mut self, now_ms: u64) -> (Sample, Vec<Effect>) {
        let mut out = Out::default();
        loop {
            let views = self.scenes.iter().map(|(id, scene)| (*id, &scene.view));
            let sampled = self.frames.borrow_mut().sample(views, now_ms);
            match sampled {
                Ok(sample) => return (sample, out.finish()),
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
        let expected =
            Frame::new().resident_bytes() + size_of::<RefCell<Frames>>() + 2 * size_of::<usize>();
        for id in 1..=4 {
            assert_eq!(frames.adopt(SceneId(id), Frame::new()), expected);
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
        assert_eq!(frames.adopt(SceneId(5), Frame::new()), expected);
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
