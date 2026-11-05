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
}
