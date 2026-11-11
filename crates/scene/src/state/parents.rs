//! Deterministic diagram ownership for compiled edges, leaving authored records unchanged.
use super::object::{self, Kind, Object};
use crate::{Op, Step, WireId, limits::MAX_OBJECT_DEPTH, text::Bounded};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn resolve(run: &mut Arc<Step>, existing: &BTreeMap<WireId, Object>) {
    if !run
        .put
        .iter()
        .any(|op| matches!(op, Op::Edge(edge) if edge.parent.is_none()))
    {
        return;
    }
    let mut hierarchy: BTreeMap<_, _> = existing
        .iter()
        .map(|(id, object)| (id, (object.kind, object.parent.as_ref())))
        .collect();
    hierarchy.extend(
        run.put
            .iter()
            .filter_map(object::model)
            .map(|visual| (visual.id, (visual.kind, visual.parent))),
    );
    let resolved: Vec<_> = run
        .put
        .iter()
        .enumerate()
        .filter_map(|(index, op)| {
            let Op::Edge(edge) = op else { return None };
            if edge.parent.is_some() {
                return None;
            }
            nearest(&edge.from, &edge.to, |id| hierarchy.get(id).copied())
                .map(|parent| (index, parent.clone()))
        })
        .collect();
    if resolved.is_empty() {
        return;
    }
    let mut put = run.put.to_vec();
    for (index, parent) in resolved {
        if let Op::Edge(edge) = &mut put[index] {
            edge.parent = Some(parent);
        }
    }
    Arc::make_mut(run).put = Bounded::new(put).expect("parent resolution preserves the op count");
}

pub(super) fn nearest<'a>(
    from: &WireId,
    to: &WireId,
    lookup: impl Fn(&WireId) -> Option<(Kind, Option<&'a WireId>)>,
) -> Option<&'a WireId> {
    let mut ancestors = [None; MAX_OBJECT_DEPTH];
    let mut cursor = lookup(from).and_then(|(_, parent)| parent);
    for slot in &mut ancestors {
        let Some(id) = cursor else { break };
        let Some((Kind::Diagram, parent)) = lookup(id) else {
            break;
        };
        *slot = Some(id);
        cursor = parent;
    }
    cursor = lookup(to).and_then(|(_, parent)| parent);
    for _ in 0..MAX_OBJECT_DEPTH {
        let Some(id) = cursor else { break };
        let Some((Kind::Diagram, parent)) = lookup(id) else {
            break;
        };
        if ancestors.contains(&Some(id)) {
            return Some(id);
        }
        cursor = parent;
    }
    None
}
