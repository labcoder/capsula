//! Queue attribution and retirement of semantic state and undo storage.

use super::{Candidate, Preparation, SceneEvent, SceneState, StoredStep, object::Object, work};
use crate::{Handle, WireId, deferred::Deferred, layout::heap, record::Point};
use std::collections::BTreeMap;

impl Candidate {
    /// Reuses the history commit will add, which also counts the step id that commit clones.
    pub(super) fn attributed_bytes(&self) -> usize {
        size_of::<Self>()
            + heap::compiled_bytes(&self.step, &self.run)
            + heap::extras_bytes(std::iter::empty(), [&self.extras])
            + self.after_bytes
            + self.history_bytes
            + self.created.capacity() * size_of::<(WireId, Handle)>()
            + heap::btree::<Handle, f32>(self.path_scales.len())
            + views_bytes(&self.views)
    }
}

/// A ticket's view updates: the vector, ids and inputs, not the sources objects share.
pub(crate) fn views_bytes(views: &Vec<crate::three::view::ViewUpdate>) -> usize {
    views.capacity() * size_of::<crate::three::view::ViewUpdate>()
        + views
            .iter()
            .map(|update| {
                update.id.as_str().len()
                    + update.input.as_ref().map_or(0, |input| input.owned_bytes())
            })
            .sum::<usize>()
}

impl Preparation {
    /// Cached bytes this ticket retains: its source, compiled input and sizing facts, not the
    /// rest of its scene. Shared allocations count once per ticket for queue attribution.
    pub fn attributed_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl SceneState {
    /// Estimated bytes this state owns, for attributing a discarded scene to the dropper's
    /// queue: objects, undo records, identifiers and the step log. Not a heap bound. Constant
    /// time: transitions keep the owned parts cached.
    pub fn attributed_bytes(&self) -> usize {
        self.objects_bytes
            + self.history_bytes
            + heap::btree::<WireId, ()>(self.reserved.len())
            + heap::btree::<WireId, u32>(self.step_ids.len())
            + heap::btree::<u32, StoredStep>(self.steps.len())
            + self.log.capacity() * size_of::<SceneEvent>()
    }

    /// The full walk the cache must equal.
    #[cfg(test)]
    fn recomputed_bytes(&self) -> usize {
        let len = |id: &WireId| id.as_str().len();
        let log: usize = self
            .log
            .iter()
            .map(|event| match event {
                SceneEvent::Step { step, .. } => heap::ARC_HEADER + heap::step_bytes(step),
                SceneEvent::Stop(cut) => size_of_val(&*cut.revealed),
            })
            .sum();
        map_bytes(&self.objects)
            + heap::btree::<WireId, ()>(self.reserved.len())
            + self.reserved.iter().map(len).sum::<usize>()
            + heap::btree::<WireId, u32>(self.step_ids.len())
            + self.step_ids.keys().map(len).sum::<usize>()
            + heap::btree::<u32, StoredStep>(self.steps.len())
            + self
                .steps
                .values()
                .map(|stored| records_bytes(&stored.removed) + records_bytes(&stored.changed))
                .sum::<usize>()
            + self.log.capacity() * size_of::<SceneEvent>()
            + log
    }
}

/// Estimated heap bytes an entry owns beyond its inline size: ids, references, motion keys and
/// series data. Admission freezes data into its own allocation, shared by later copies, so this
/// counts it once per holder rather than once per allocation; `usage.marks` is its length.
pub(super) fn owned((id, object): (&WireId, &Object)) -> usize {
    let len = |id: &WireId| id.as_str().len();
    len(id)
        + object.parent.as_ref().map_or(0, len)
        + object.work.chart_owner.as_ref().map_or(0, len)
        + size_of_val(&*object.references)
        + object.references.iter().map(len).sum::<usize>()
        + heap::btree::<&str, work::Motion>(object.work.motion.len())
        + object.work.data.as_ref().map_or(0, |_| {
            heap::ARC_HEADER + object.work.usage.marks * size_of::<Point>()
        })
        + object
            .three
            .as_ref()
            .map_or(0, |three| heap::source_bytes(three))
}

/// Estimated bytes of a semantic object map, for queue attribution rather than a bound.
pub(super) fn map_bytes(objects: &BTreeMap<WireId, Object>) -> usize {
    heap::btree::<WireId, Object>(objects.len()) + objects.iter().map(owned).sum::<usize>()
}

/// Estimated bytes of undo records, for queue attribution rather than a bound.
pub(super) fn records_bytes(records: &Vec<(WireId, Object)>) -> usize {
    records.capacity() * size_of::<(WireId, Object)>()
        + records
            .iter()
            .map(|(id, object)| owned((id, object)))
            .sum::<usize>()
}

/// Re-attributes a queued object map from its cached owned bytes, as `map_bytes` would.
pub(super) fn attribute(map: &mut Deferred<BTreeMap<WireId, Object>>, owned: usize) {
    let bytes = heap::btree::<WireId, Object>(map.len()) + owned;
    map.attribute(bytes);
}

/// Hands undo records to the dropper, skipping empty ones that own nothing, and returns the
/// bytes they released from the state's history.
pub(super) fn retire_undo(undo: impl IntoIterator<Item = Vec<(WireId, Object)>>) -> usize {
    let undo: Vec<_> = undo
        .into_iter()
        .filter(|records| records.capacity() > 0)
        .collect();
    let bytes = undo.iter().map(records_bytes).sum();
    if !undo.is_empty() {
        drop(Deferred::new(undo, bytes));
    }
    bytes
}

#[cfg(test)]
mod tests {
    use crate::{
        Admission, GlobalBudget, Record, SceneState, StopCut, WireId, parse_record, text::Bounded,
    };

    fn commit(scene: &mut SceneState, budget: &mut GlobalBudget, step: &str) {
        let record = format!(r#"{{"step":{step}}}"#);
        let Record::Step(step) = parse_record(record.as_bytes()).expect("typed step") else {
            panic!("step fixture");
        };
        let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted step") else {
            panic!("new step");
        };
        scene.commit(candidate, budget).expect("fresh admission");
    }

    fn check(scene: &SceneState, after: &str) {
        assert_eq!(
            scene.attributed_bytes(),
            scene.recomputed_bytes(),
            "the cache equals a full walk after {after}"
        );
    }

    #[test]
    fn cached_attribution_matches_a_full_walk_after_every_transition() {
        let Record::Begin(begin) =
            parse_record(br#"{"begin":{"v":1,"scene":"cache","surface":"inline","alt":""}}"#)
                .expect("begin record")
        else {
            panic!("begin fixture");
        };
        let mut scene = SceneState::begin(begin.clone()).expect("scene");
        let mut budget = GlobalBudget::default();
        check(&scene, "begin");
        commit(
            &mut scene,
            &mut budget,
            r#"{"id":"one","put":[{"table":{"id":"t","cols":["A"],"box":[0,0,300,300]}},{"row":{"id":"r0","in":"t","cells":["a"]}},{"row":{"id":"r1","in":"t","cells":["b"]}},{"shape":{"id":"s0","kind":"rect","box":[0,0,1,1]}},{"shape":{"id":"s1","kind":"rect","box":[0,0,1,1]}},{"line":{"id":"l","from":{"ref":"s0"},"to":[1,1]}},{"chart":{"id":"c","kind":"line"}},{"series":{"id":"p","in":"c","data":[1,2,3]}}]}"#,
        );
        check(&scene, "a first commit");
        commit(
            &mut scene,
            &mut budget,
            r#"{"id":"two","put":[{"shape":{"id":"s2","kind":"rect","box":[0,0,1,1]}},{"anim":{"target":"s0","track":"opacity","keys":[[0,0],[100,1]]}}],"set":[{"target":"r1","cells":["c"]},{"target":"p","data":[4,5]}],"remove":["r0"]}"#,
        );
        check(
            &scene,
            "a commit with tracks, cell and data changes and a removal",
        );
        commit(
            &mut scene,
            &mut budget,
            r#"{"id":"three","put":[{"shape":{"id":"s3","kind":"rect","box":[0,0,1,1]}},{"anim":{"target":"s1","track":"x","keys":[[0,0],[100,1]]}}]}"#,
        );
        check(&scene, "a third commit");
        scene
            .retire(1, &mut budget)
            .expect("retire the second step");
        check(&scene, "retiring undo records");
        let kept = ["t", "r1", "s0", "s1", "l", "c", "p", "s2"].map(|id| {
            scene
                .handle(&WireId::try_from(id.to_owned()).expect("id"))
                .expect("live")
        });
        let cut = StopCut {
            activated: 2,
            revealed: Bounded::new(kept.to_vec()).expect("bounded handles"),
        };
        scene.stop(&cut, &mut budget).expect("drop the third step");
        assert_eq!(scene.object_count(), 8, "the cut drops s3");
        check(&scene, "a Stop that drops a step");
        scene.stop(&cut, &mut budget).expect("repeat the cut");
        check(&scene, "a Stop with no effect");
        let replayed = SceneState::replay(begin, scene.recovery_log()).expect("replay");
        check(&replayed, "replay");
        scene.clear(&mut budget).expect("clear");
        check(&scene, "clear");
    }
}
