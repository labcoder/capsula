//! The current text of a clicked 3D part, for asking about it.

use super::{SceneState, object::Kind};
use crate::{Handle, Op, Text};
use std::fmt;

/// A live labelled solid's current text, borrowed from the kept log.
pub struct Part<'a> {
    /// Its display label.
    pub label: &'a Text,
    /// Its descriptive text, when it has any.
    pub info: Option<&'a Text>,
}

impl fmt::Debug for Part<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Part")
            .field("label", self.label)
            .field("info", &self.info)
            .finish()
    }
}

impl SceneState {
    /// The surviving solid with `handle` and a label: its put, with every later kept step's
    /// `set` of its label or info applied in order. `None` for a cleared scene, a removed or
    /// unknown handle, any other kind, or a solid without a label.
    pub fn part(&self, handle: Handle) -> Option<Part<'_>> {
        if self.cleared {
            return None;
        }
        let (id, _) = self
            .objects
            .iter()
            .find(|(_, object)| object.handle == handle && object.kind == Kind::Solid)?;
        let revision = handle.revision();
        let put = self.steps.get(&revision).filter(|stored| stored.kept)?;
        let solid = self
            .original(put.event)
            .put
            .iter()
            .find_map(|op| match op {
                Op::Solid(solid) if solid.id == *id => Some(solid),
                _ => None,
            })?;
        let (mut label, mut info) = (solid.label.as_ref(), solid.info.as_ref());
        for stored in self.steps.range(revision + 1..).map(|(_, stored)| stored) {
            if !stored.kept {
                continue;
            }
            for change in self.original(stored.event).set.iter() {
                if change.target == *id {
                    label = change.label.as_ref().or(label);
                    info = change.info.as_ref().or(info);
                }
            }
        }
        Some(Part {
            label: label?,
            info,
        })
    }
}
