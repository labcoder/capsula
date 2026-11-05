use std::cell::RefCell;

use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_scene::Frame;

pub(super) struct Slot {
    frame: Frame,
}

#[derive(Default)]
pub(crate) struct Frames {
    slots: [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES],
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
        let place = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("each adopted scene has one of the four reserved positions");
        *place = Some((scene, Slot { frame }));
        self.slots
            .sort_unstable_by_key(|slot| slot.as_ref().map(|(id, _)| *id));
        bytes
    }

    pub(super) fn replaced(&mut self, scene: SceneId) {
        if let Some((_, slot)) = self.slots.iter_mut().flatten().find(|(id, _)| *id == scene) {
            slot.frame.clear();
        }
    }

    pub(super) fn take(&mut self, scene: SceneId) -> Option<Slot> {
        self.slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(id, _)| *id == scene))
            .and_then(Option::take)
            .map(|(_, slot)| slot)
    }

    pub(super) fn take_all(&mut self) -> [Option<(SceneId, Slot)>; MAX_VISIBLE_SCENES] {
        std::mem::take(&mut self.slots)
    }

    #[cfg(test)]
    pub(super) fn ids(&self) -> [Option<SceneId>; MAX_VISIBLE_SCENES] {
        self.slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|(id, _)| *id))
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
