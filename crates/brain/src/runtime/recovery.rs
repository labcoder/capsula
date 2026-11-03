//! One connection's recovery: after `Scenes`, the kept steps of every visible scene, copied so
//! later changes can't alter a snapshot in flight, go to the data queue one chunk at a time.

use std::collections::VecDeque;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{DataToShell, SceneId, SceneSnapshotChunk, Step};

use crate::store::SceneState;

/// What a chunk packs up to [guess]. A target, not a maximum: a bigger step goes alone.
pub(super) const CHUNK_TARGET: usize = 32 * 1024;
/// A step's encoding beyond its label: the label's length prefix, at most 2 bytes for a label
/// under 16 KiB.
const STEP_OVERHEAD: usize = 2;

#[derive(Debug)]
pub(super) struct Recovery {
    drawings: VecDeque<SceneSnapshotChunk>,
    /// Each scene with the revision its descriptor named, oldest first.
    scenes: VecDeque<(SceneId, u32, Vec<Step>)>,
    /// How many of the front scene's steps have gone out.
    sent: usize,
    target: usize,
}

impl Recovery {
    pub(super) fn new(visible: &[SceneState], target: usize) -> Self {
        debug_assert!(
            visible.iter().all(|state| !state.kept.is_empty()),
            "a settled visible scene keeps at least one step"
        );
        Self {
            drawings: VecDeque::new(),
            scenes: visible
                .iter()
                .map(|state| (state.scene, state.revision, state.kept.clone()))
                .collect(),
            sent: 0,
            target,
        }
    }

    pub(super) fn drawings(chunks: Vec<SceneSnapshotChunk>) -> Self {
        Self {
            drawings: chunks.into(),
            scenes: VecDeque::new(),
            sent: 0,
            target: CHUNK_TARGET,
        }
    }

    /// Chunks are still to go out.
    pub(super) fn pending(&self) -> bool {
        !self.scenes.is_empty() || !self.drawings.is_empty()
    }

    /// The next chunk: whole steps up to the target, never empty.
    pub(super) fn next_chunk(&mut self) -> Option<DataToShell> {
        if let Some(chunk) = self.drawings.pop_front() {
            return Some(DataToShell::SceneSnapshotV1 { chunk });
        }
        let (scene, revision, steps) = self.scenes.front()?;
        let (scene, revision) = (*scene, *revision);
        let mut bytes = 0;
        let mut end = self.sent;
        while let Some(step) = steps.get(end) {
            let size = step.label.len() + STEP_OVERHEAD;
            if end > self.sent && bytes + size > self.target {
                break;
            }
            bytes += size;
            end += 1;
        }
        debug_assert!(
            bytes < Pipe::Data.max_payload(),
            "labels are capped far below a data frame"
        );
        let chunk = steps[self.sent..end].to_vec();
        let last = end == steps.len();
        if last {
            self.scenes.pop_front();
            self.sent = 0;
        } else {
            self.sent = end;
        }
        Some(DataToShell::SceneSnapshot {
            scene,
            revision,
            chunk,
            last,
        })
    }

    /// The shell cleared these scenes, so nothing more of them goes out.
    pub(super) fn drop_scenes(&mut self, cleared: &[SceneId]) {
        self.drawings
            .retain(|chunk| !cleared.contains(&chunk.scene));
        if self
            .scenes
            .front()
            .is_some_and(|(scene, _, _)| cleared.contains(scene))
        {
            self.sent = 0;
        }
        self.scenes.retain(|(scene, _, _)| !cleared.contains(scene));
    }
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::{DataToShell, RunId, SceneId, Step, ThreadId};

    use super::Recovery;
    use crate::store::SceneState;

    fn scene(id: u64, labels: &[&str]) -> SceneState {
        SceneState {
            scene: SceneId(id),
            thread: ThreadId(1),
            run: RunId(0),
            stopped: false,
            revision: 9,
            kept: labels
                .iter()
                .map(|label| Step {
                    label: (*label).to_owned(),
                })
                .collect(),
        }
    }

    /// Each chunk as its scene, labels and `last`.
    fn drain(recovery: &mut Recovery) -> Vec<(u64, Vec<String>, bool)> {
        std::iter::from_fn(|| recovery.next_chunk())
            .map(|chunk| {
                let DataToShell::SceneSnapshot {
                    scene, chunk, last, ..
                } = chunk
                else {
                    panic!("only snapshots");
                };
                (
                    scene.0,
                    chunk.into_iter().map(|step| step.label).collect(),
                    last,
                )
            })
            .collect()
    }

    fn labels(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|label| (*label).to_owned()).collect()
    }

    #[test]
    fn chunks_pack_whole_steps_up_to_the_target_and_a_bigger_step_goes_alone() {
        let big = "b".repeat(20);
        let visible = [scene(3, &["aa", "aa", &big, "cc"]), scene(5, &["dd"])];
        // Each two-letter step costs 4 bytes, so two fit an 8-byte target.
        let mut recovery = Recovery::new(&visible, 8);
        assert_eq!(
            drain(&mut recovery),
            [
                (3, labels(&["aa", "aa"]), false),
                (3, vec![big], false),
                (3, labels(&["cc"]), true),
                (5, labels(&["dd"]), true),
            ]
        );
        assert!(!recovery.pending());
    }

    #[test]
    fn a_cleared_scene_stops_mid_snapshot_and_the_next_starts_whole() {
        let visible = [scene(3, &["a", "b", "c"]), scene(5, &["d", "e"])];
        let mut recovery = Recovery::new(&visible, 1);
        assert_eq!(drain_one(&mut recovery), (3, labels(&["a"]), false));
        recovery.drop_scenes(&[SceneId(3)]);
        assert_eq!(
            drain(&mut recovery),
            [(5, labels(&["d"]), false), (5, labels(&["e"]), true)]
        );
    }

    fn drain_one(recovery: &mut Recovery) -> (u64, Vec<String>, bool) {
        let DataToShell::SceneSnapshot {
            scene, chunk, last, ..
        } = recovery.next_chunk().expect("a chunk")
        else {
            panic!("only snapshots");
        };
        (
            scene.0,
            chunk.into_iter().map(|step| step.label).collect(),
            last,
        )
    }
}
