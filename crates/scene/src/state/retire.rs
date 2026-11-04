//! Queue attribution and retirement of semantic state and undo storage.

use super::{Candidate, SceneEvent, SceneState, StoredStep, object::Object, work};
use crate::{Handle, WireId, deferred::Deferred, layout::heap};
use std::collections::BTreeMap;

impl Candidate {
    pub(super) fn attributed_bytes(&self) -> usize {
        size_of::<Self>()
            + heap::ARC_HEADER
            + heap::step_bytes(&self.step)
            + heap::compiled_bytes(&self.step, &self.run)
            + heap::extras_bytes(std::iter::empty(), [&self.extras])
            + map_bytes(&self.after)
            + records_bytes(&self.removed)
            + records_bytes(&self.changed)
            + self.created.capacity() * size_of::<(WireId, Handle)>()
            + self
                .created
                .iter()
                .map(|(id, _)| id.as_str().len())
                .sum::<usize>()
    }
}

impl SceneState {
    /// Estimated bytes this state owns, for attributing a discarded scene to the dropper's
    /// queue: objects, undo records, identifiers and the step log. Not a heap bound.
    pub fn attributed_bytes(&self) -> usize {
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

/// Estimated heap bytes an entry owns beyond its inline size: ids, references and motion keys.
/// Series data stays shared with the step log, so it is not counted.
fn owned((id, object): (&WireId, &Object)) -> usize {
    let len = |id: &WireId| id.as_str().len();
    len(id)
        + object.parent.as_ref().map_or(0, len)
        + object.work.chart_owner.as_ref().map_or(0, len)
        + object.references.capacity() * size_of::<WireId>()
        + object.references.iter().map(len).sum::<usize>()
        + heap::btree::<&str, work::Motion>(object.work.motion.len())
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

/// Hands undo records to the dropper, skipping empty ones that own nothing.
pub(super) fn retire_undo(undo: impl IntoIterator<Item = Vec<(WireId, Object)>>) {
    let undo: Vec<_> = undo
        .into_iter()
        .filter(|records| records.capacity() > 0)
        .collect();
    if !undo.is_empty() {
        let bytes = undo.iter().map(records_bytes).sum();
        drop(Deferred::new(undo, bytes));
    }
}
