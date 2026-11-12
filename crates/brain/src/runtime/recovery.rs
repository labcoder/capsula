//! One connection's recovery: after `Scenes`, the kept steps of every visible scene, copied so
//! later changes can't alter a snapshot in flight, go to the data queue one chunk at a time.

use std::{collections::VecDeque, fmt};

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{DataToShell, SceneId, SceneSnapshotChunk, Step};

use crate::{drawing::CHUNK_TARGET, store::SceneState};

/// A step's encoding beyond its label: the label's length prefix, at most 2 bytes for a label
/// under 16 KiB.
const STEP_OVERHEAD: usize = 2;

pub(super) struct Recovery {
    drawings: VecDeque<SceneSnapshotChunk>,
    /// Each scene with the revision its descriptor named, oldest first.
    scenes: VecDeque<(SceneId, u32, Vec<Step>)>,
    /// How many of the front scene's steps have gone out.
    sent: usize,
    target: usize,
}

impl fmt::Debug for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recovery")
            .field("drawings", &self.drawings.len())
            .field("scenes", &self.scenes.len())
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
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
    use std::sync::Arc;

    use kumochi_protocol::frame::{self, Pipe};
    use kumochi_protocol::message::{
        DataToShell, RunId, SceneHeader, SceneId, SceneSnapshotChunk, Step, ThreadId,
    };
    use kumochi_scene::{Handle, SceneEvent, StopCut, text::Bounded};

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

    #[test]
    fn typed_recovery_debug_is_count_only_and_clear_drops_remaining_scene_chunks() {
        let header: SceneHeader = serde_json::from_value(serde_json::json!({
            "begin": {"v":1,"scene":"private-scene","surface":"overlay","alt":"private-alt"},
            "layout_version":2
        }))
        .expect("typed header");
        let event = |revision, id| {
            SceneEvent::Step {
            revision,
            step: Arc::new(serde_json::from_value(serde_json::json!({
                "id":id,"say":"private-narration",
                "put":[{"shape":{"id":format!("object-{id}"),"kind":"rect","box":[13337.25,1,10,10]}}]
            })).expect("typed step")),
        }
        };
        let cut = SceneEvent::Stop(StopCut {
            activated: 1,
            revealed: Bounded::new(vec![Handle::new(1, 0).expect("handle")]).expect("handles"),
        });
        let chunk = |scene, ordinal, events, last| {
            SceneSnapshotChunk {
                scene: SceneId(scene),
                revision: 3,
                ordinal,
                header: (ordinal == 0).then(|| header.clone()),
                events: Bounded::new(events).expect("bounded events"),
                last,
            }
            .checked()
            .expect("valid snapshot chunk")
        };
        let first = chunk(3, 0, vec![event(1, "a1")], false);
        let kept_first = chunk(5, 0, vec![event(1, "b1"), event(2, "b2")], false);
        let mut put =
            vec![serde_json::json!({"chart":{"id":"dense","kind":"line","box":[0,0,100,100]}})];
        for index in 0..8 {
            put.push(serde_json::json!({"series":{"id":format!("series-{index}"),"in":"dense","data":vec![51.248_178_375_505_404;200]}}));
        }
        let bytes = serde_json::to_vec(&serde_json::json!({"step":{"id":"b3","put":put}}))
            .expect("model JSON");
        let kumochi_scene::Record::Step(step) =
            kumochi_scene::parse_record(&bytes).expect("legal dense model record")
        else {
            panic!("step")
        };
        let dense = SceneEvent::Step {
            revision: 3,
            step: Arc::new(step),
        };
        let kept_last = chunk(5, 1, vec![cut.clone(), dense.clone()], true);
        let mut recovery = Recovery::drawings(vec![
            first.clone(),
            chunk(3, 1, vec![event(2, "a2"), cut.clone()], false),
            kept_first.clone(),
            chunk(3, 2, vec![event(3, "a3")], true),
            kept_last.clone(),
        ]);
        assert_eq!(
            format!("{recovery:?}"),
            "Recovery { drawings: 5, scenes: 0, sent: 0, .. }"
        );
        assert_eq!(
            recovery.next_chunk(),
            Some(DataToShell::SceneSnapshotV1 { chunk: first })
        );
        recovery.drop_scenes(&[SceneId(3)]);
        let mut log = Vec::new();
        for expected in [kept_first, kept_last] {
            let message = recovery.next_chunk().expect("surviving scene chunk");
            let bytes = frame::encode(&message, Pipe::Data).expect("whole-event data frame");
            let decoded = frame::read::<DataToShell>(&mut bytes.as_slice(), Pipe::Data)
                .expect("decode snapshot frame")
                .expect("one snapshot frame");
            assert_eq!(decoded, DataToShell::SceneSnapshotV1 { chunk: expected });
            let DataToShell::SceneSnapshotV1 { chunk } = decoded else {
                panic!("only typed snapshots");
            };
            log.extend(chunk.events.iter().cloned());
        }
        assert_eq!(log, vec![event(1, "b1"), event(2, "b2"), cut, dense]);
        kumochi_scene::SceneState::replay(header.begin, &log)
            .expect("the survivor's full log replays with its cut and revision gap");
        assert_eq!(recovery.next_chunk(), None);
        assert!(!recovery.pending());
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
