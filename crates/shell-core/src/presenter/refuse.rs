//! Scene traffic for a shell that draws nothing: credit every frame, refuse what a call draws.

use std::collections::VecDeque;

use kumochi_protocol::message::{ControlToBrain, SceneId};

use super::{CLEARED_KEPT, Effect};

/// What one delivered data frame means to a shell that draws nothing. The link's
/// [`scene_naming`](crate::link::scene_naming) builds it from the borrowed frame, beside the
/// test that tags scene frames, so the two can't disagree about a kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming<'a> {
    /// A call draws on this scene: `SceneBegin`, `SceneStepCommit`.
    Draws(SceneId),
    /// Retained state for this scene with no call: a `SceneSnapshotV1` chunk.
    Retained(SceneId),
    /// These scenes are gone: a zero-activated cut, `ScenesRemoved`, the victims of
    /// `ScenesReplaced`.
    Retires(&'a [SceneId]),
    /// `SceneEnd`, a nonzero cut, and every frame that isn't scene traffic: credit only.
    Other,
}

/// Answers scene traffic for a shell with no drawing surface, so the brain is never stalled by
/// it: every data frame the open connection delivers is credited back exactly once, or the data
/// writer stops after one window, and a scene a call draws on is refused with `SceneFailed` the
/// first time, or that call waits on a scene nobody shows. Plain state with no clock and no I/O;
/// the caller runs the returned effects in order.
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
    /// A new connection opened; the last connection's refusals are forgotten.
    pub fn connected(&mut self, generation: u64) {
        self.generation = Some(generation);
        self.refused.clear();
    }

    /// The connection ended; nothing more is credited or refused until the next one.
    pub fn disconnected(&mut self) {
        self.generation = None;
        self.refused.clear();
    }

    /// One delivered `Data` or `SceneData` frame of `bytes` on the wire: exactly one
    /// `Credit(bytes)` for the open generation, then `SceneFailed` the first time a call draws on
    /// a scene. A stale generation, or no connection, gets nothing.
    pub fn data(&mut self, generation: u64, naming: Naming<'_>, bytes: u32) -> Vec<Effect> {
        if self.generation != Some(generation) {
            return Vec::new();
        }
        let mut effects = vec![Effect::Credit(bytes)];
        match naming {
            Naming::Draws(scene) if !self.refused.contains(&scene) => {
                if self.refused.len() == CLEARED_KEPT {
                    self.refused.pop_front();
                }
                self.refused.push_back(scene);
                effects.push(Effect::Control(ControlToBrain::SceneFailed { scene }));
            }
            Naming::Retires(scenes) => self.refused.retain(|scene| !scenes.contains(scene)),
            Naming::Draws(_) | Naming::Retained(_) | Naming::Other => {}
        }
        effects
    }
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod tests;
