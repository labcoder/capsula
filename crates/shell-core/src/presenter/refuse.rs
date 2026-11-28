//! The contract for scene traffic in a shell without a drawing surface: return every frame's
//! credit, and refuse a scene a call draws on.

use std::collections::VecDeque;

use kumochi_protocol::message::SceneId;

use super::CLEARED_KEPT;

/// What one delivered data frame means to a shell that draws nothing. The link's
/// [`scene_naming`](crate::link::scene_naming) builds it from the borrowed frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming<'a> {
    /// A call draws on this scene: `SceneBegin`, `SceneStepCommit`.
    Draws(SceneId),
    /// Retained state for this scene with no call: a `SceneSnapshotV1` chunk.
    Retained(SceneId),
    /// These scenes are gone: a zero-activated cut, `ScenesRemoved`, the victims of
    /// `ScenesReplaced`.
    Retires(&'a [SceneId]),
    /// `SceneEnd`, a nonzero cut, and every frame `scene_naming` doesn't name: credit only.
    Other,
}

/// What the caller does with one frame, in order: grant `credit` on the frame's connection, then
/// queue `SceneFailed` for `refuse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "the brain's data writer waits for this credit"]
pub struct Disposal {
    /// The frame's wire bytes.
    pub credit: u32,
    /// A scene a call draws on, refused for the first time.
    pub refuse: Option<SceneId>,
}

/// The contract for a shell with no drawing surface: how it answers scene traffic so the brain
/// isn't stalled by it. `kumochi-nsh` is its caller. The caller passes every delivered `Data` and
/// `SceneData` frame once, with [`Naming::Other`] when `scene_naming` returns `None`; control
/// lists such as `ScenesV1` and legacy `Scenes` owe nothing and never reach it. For the open
/// connection each frame's credit comes back for the caller to grant, or the data writer stops
/// after one window, and a scene a call draws on is refused the first time, or that call waits
/// on a scene nobody shows. Plain state with no clock, no I/O and no allocation per frame.
/// `true` from `LinkHandle::send_control` means the mailbox took a refusal, not that it was
/// delivered: a pipe that ends first can still drop it, and the connection is ending then anyway.
///
/// The brain's `SceneFailed` ends the whole scene, so a call that draws onto a retained scene
/// loses that stored drawing with it. That is acceptable only because `kumochi-nsh` refuses the
/// release identity: only Dev and Gate data can be affected. Retained chunks alone are credited
/// and left alone. Route B's real presenter (RB-8) replaces this type.
///
/// Refusals are remembered until their scene retires, at most [`CLEARED_KEPT`] of them; past that
/// the oldest is forgotten. A later draw on a forgotten scene is refused again, which is harmless:
/// the brain ignores `SceneFailed` for a scene it no longer holds.
#[derive(Debug, Default)]
pub struct Refuser {
    generation: Option<u64>,
    /// Scenes refused on this connection and not yet retired, oldest first.
    refused: VecDeque<SceneId>,
}

impl Refuser {
    /// A new connection opened; the last connection's refusals are forgotten, and room for every
    /// refusal is reserved here rather than per frame.
    pub fn connected(&mut self, generation: u64) {
        self.generation = Some(generation);
        self.refused.clear();
        self.refused.reserve(CLEARED_KEPT);
    }

    /// The connection ended; nothing more is credited or refused until the next one.
    pub fn disconnected(&mut self) {
        self.generation = None;
        self.refused.clear();
    }

    /// One delivered frame of `bytes` on the wire: for the open generation, its credit and the
    /// scene to refuse the first time a call draws on it. `None` for a stale generation or no
    /// connection.
    pub fn data(&mut self, generation: u64, naming: Naming<'_>, bytes: u32) -> Option<Disposal> {
        if self.generation != Some(generation) {
            return None;
        }
        let mut refuse = None;
        match naming {
            Naming::Draws(scene) if !self.refused.contains(&scene) => {
                if self.refused.len() == CLEARED_KEPT {
                    self.refused.pop_front();
                }
                self.refused.push_back(scene);
                refuse = Some(scene);
            }
            Naming::Retires(scenes) => self.refused.retain(|scene| !scenes.contains(scene)),
            Naming::Draws(_) | Naming::Retained(_) | Naming::Other => {}
        }
        Some(Disposal {
            credit: bytes,
            refuse,
        })
    }
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod tests;
