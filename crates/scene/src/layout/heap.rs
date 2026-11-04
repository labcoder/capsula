//! Requested heap bytes scene values hold, pinned to Rust 1.98's collection layouts.

use super::state::Placed;
use crate::{
    Op, Step, Text, WireId,
    mermaid::Extra,
    record::{Anchor, Axis, Cell, Change, Start},
    state::object,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// An `Arc` allocation's strong and weak counts, which precede its value.
pub(crate) const ARC_HEADER: usize = 2 * size_of::<usize>();

/// Upper bound on the requested bytes of a `BTreeMap<K, V>` or `BTreeSet<K>` (`V = ()`) with
/// `len` entries, pinned to Rust 1.98's B-tree: a node holds 11 entries, every node but the
/// root keeps at least 5, an internal node adds 12 edges, and up to 10 entries share one root
/// leaf. A map that removals emptied keeps a root leaf this does not count.
// shortcut: std's node layout is private; the allocation-counter tests recheck this bound on
// each toolchain bump.
pub(crate) fn btree<K, V>(len: usize) -> usize {
    const CAPACITY: usize = 11;
    const MIN_LEN: usize = 5;
    let align = align_of::<usize>()
        .max(align_of::<K>())
        .max(align_of::<V>());
    let header = size_of::<usize>() + 2 * size_of::<u16>();
    let leaf = (header + CAPACITY * (size_of::<K>() + size_of::<V>())).next_multiple_of(align);
    let internal = (leaf + (CAPACITY + 1) * size_of::<usize>()).next_multiple_of(align);
    match len {
        0 => 0,
        1..=10 => leaf,
        _ => (1 + len / MIN_LEN) * internal,
    }
}

/// Bytes of the distinct `Op` copies in `objects` that `shared` doesn't already hold.
pub(super) fn source_bytes<'a>(
    shared: impl IntoIterator<Item = &'a Placed>,
    objects: impl IntoIterator<Item = &'a Placed>,
) -> usize {
    let mut seen: HashSet<_> = shared
        .into_iter()
        .map(|object| Arc::as_ptr(&object.source))
        .collect();
    objects
        .into_iter()
        .filter(|object| seen.insert(Arc::as_ptr(&object.source)))
        .map(|object| size_of::<Op>() + op_bytes(&object.source))
        .sum()
}

/// Estimated bytes a committed step owns: its records and the strings and sequences inside them,
/// at their lengths rather than their capacities.
pub(crate) fn step_bytes(step: &Step) -> usize {
    let id = |id: &WireId| id.as_str().len();
    size_of::<Step>()
        + id(&step.id)
        + optional(step.say.as_ref())
        + step
            .put
            .iter()
            .map(|op| size_of::<Op>() + op_bytes(op))
            .sum::<usize>()
        + step.set.iter().map(change_bytes).sum::<usize>()
        + step
            .remove
            .iter()
            .map(|removed| size_of::<WireId>() + id(removed))
            .sum::<usize>()
        + match &step.start {
            Start::After { step, .. } => id(step),
            Start::Speech | Start::Now => 0,
        }
}

fn change_bytes(change: &Change) -> usize {
    size_of::<Change>()
        + change.target.as_str().len()
        + optional(change.text.as_ref())
        + optional(change.label.as_ref())
        + change.cells.as_deref().map_or(0, cells)
        + change.data.as_deref().map_or(0, elements)
}

/// The compiled record a prepared ticket owns, unless it shares the original log record.
pub(crate) fn compiled_bytes(original: &Arc<Step>, run: &Arc<Step>) -> usize {
    if Arc::ptr_eq(original, run) {
        return 0;
    }
    step_bytes(run)
}

pub(crate) fn extras_bytes<'a>(
    shared: impl IntoIterator<Item = &'a Arc<BTreeMap<usize, Extra>>>,
    extras: impl IntoIterator<Item = &'a Arc<BTreeMap<usize, Extra>>>,
) -> usize {
    let mut seen: HashSet<_> = shared.into_iter().map(Arc::as_ptr).collect();
    extras
        .into_iter()
        .filter(|extras| seen.insert(Arc::as_ptr(extras)))
        .map(|extras| {
            ARC_HEADER
                + size_of::<BTreeMap<usize, Extra>>()
                + btree::<usize, Extra>(extras.len())
                + extras
                    .values()
                    .map(|extra| extra.bytes() - size_of::<(usize, Extra)>())
                    .sum::<usize>()
        })
        .sum()
}

/// Heap bytes an operation owns: each string's bytes and each sequence's elements at their
/// own size. Exact for layout copies, because cloned records have exact capacities.
fn op_bytes(op: &Op) -> usize {
    let ids = object::visual(op).map_or(0, |visual| {
        visual.id.as_str().len() + visual.parent.map_or(0, |parent| parent.as_str().len())
    });
    ids + match op {
        Op::Line(line) | Op::Arrow(line) => {
            anchor(&line.from) + anchor(&line.to) + elements(&line.via)
        }
        Op::Path(path) => path.d.as_str().len() + path.at.as_ref().map_or(0, anchor),
        Op::Shape(shape) => {
            shape.at.as_ref().map_or(0, anchor)
                + elements(&shape.points)
                + optional(shape.label.as_ref())
        }
        Op::Text(label) => anchor(&label.at) + label.text.as_str().len(),
        Op::Callout(callout) => {
            anchor(&callout.target)
                + callout.at.as_ref().map_or(0, anchor)
                + callout.text.as_str().len()
        }
        Op::Highlight(highlight) => anchor(&highlight.target),
        Op::Diagram(_) => 0,
        Op::Node(node) => node.label.as_str().len() + optional(node.sub.as_ref()),
        Op::Edge(edge) => {
            edge.from.as_str().len() + edge.to.as_str().len() + optional(edge.label.as_ref())
        }
        Op::Table(table) => {
            texts(&table.cols) + elements(&table.align) + optional(table.title.as_ref())
        }
        Op::Row(row) => cells(&row.cells),
        Op::Chart(chart) => {
            let axis = |side: &Axis| optional(side.label.as_ref()) + optional(side.unit.as_ref());
            optional(chart.title.as_ref())
                + texts(&chart.categories)
                + axis(&chart.x)
                + axis(&chart.y)
        }
        Op::Series(series) => optional(series.name.as_ref()) + elements(&series.data),
        Op::Anim(anim) => anim.target.as_str().len() + elements(&anim.keys),
        Op::Mermaid(mermaid) => mermaid.parent.as_str().len() + mermaid.src.as_str().len(),
    }
}

fn cells(values: &[Cell]) -> usize {
    elements(values)
        + values
            .iter()
            .map(|cell| match cell {
                Cell::Text(text) => text.as_str().len(),
                Cell::Number(_) | Cell::Bool(_) => 0,
            })
            .sum::<usize>()
}

/// A sequence's elements at their own inline size.
fn elements<T>(values: &[T]) -> usize {
    size_of_val(values)
}
fn texts(values: &[Text]) -> usize {
    elements(values)
        + values
            .iter()
            .map(|value| value.as_str().len())
            .sum::<usize>()
}
fn optional(value: Option<&Text>) -> usize {
    value.map_or(0, |value| value.as_str().len())
}
fn anchor(value: &Anchor) -> usize {
    match value {
        Anchor::At(_) => 0,
        Anchor::Ref(target) => target.as_str().len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Finite, Record, WireId, parse_record,
        record::{Change, Point},
    };

    fn op(json: &str) -> Op {
        let record = format!(r#"{{"step":{{"id":"s","put":[{json}]}}}}"#);
        let Record::Step(step) = parse_record(record.as_bytes()).expect("typed step") else {
            panic!("step");
        };
        step.put[0].clone()
    }

    #[test]
    fn sources_charge_owned_sequence_elements_at_their_own_size() {
        assert_eq!(
            op_bytes(&op(r#"{"series":{"id":"s","in":"c","data":[1,2,3]}}"#)),
            2 + 3 * size_of::<Point>(),
            "two one-byte ids and three owned points"
        );
        assert_eq!(
            op_bytes(&op(
                r#"{"chart":{"id":"c","kind":"bar","box":[0,0,400,300],"categories":["ab","c"],"x":{"min":-0.5,"max":1.5,"label":"X"},"y":{"min":-5,"max":5}}}"#
            )),
            2 * size_of::<Text>() + 5,
            "the id, two owned category texts with their three bytes, and the x label"
        );
        assert_eq!(
            op_bytes(&op(r#"{"row":{"id":"r","in":"t","cells":["ab",2,true]}}"#)),
            3 * size_of::<Cell>() + 4,
            "two ids, three owned cells and the text cell's bytes"
        );
        assert_eq!(
            op_bytes(&op(
                r#"{"line":{"id":"l","from":{"ref":"a"},"to":[1,1],"via":[[0,1],[1,0]]}}"#
            )),
            2 * size_of::<[Finite; 2]>() + 2,
            "the id, the referenced id and two owned via points"
        );
    }

    #[test]
    fn compiled_records_charge_owned_fields_but_not_a_shared_log_record() {
        let Record::Step(record) = parse_record(br#"{"step":{"id":"s","say":"say","put":[{"shape":{"id":"p","kind":"rect","box":[0,0,1,1]}}],"set":[{"target":"p","text":"tt","label":"lll","cells":["cc",1],"data":[1,2]}],"remove":["zz"],"start":{"after":"before","ms":0}}}"#).expect("typed record") else { panic!("step"); };
        let original = Arc::new(record);
        assert_eq!(compiled_bytes(&original, &original), 0);
        let copied = Arc::new((*original).clone());
        assert_eq!(
            compiled_bytes(&original, &copied),
            size_of::<Step>()
                + size_of::<Op>()
                + size_of::<Change>()
                + size_of::<WireId>()
                + 2 * size_of::<Cell>()
                + 2 * size_of::<Point>()
                + 21
        );
    }
}
