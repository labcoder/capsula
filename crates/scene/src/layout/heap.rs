//! Heap bytes held by a layout's own copies of record sources.

use super::state::Placed;
use crate::{
    Op, Text,
    record::{Anchor, Axis, Cell},
    state::object,
};
use std::{collections::HashSet, sync::Arc};

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
        Op::Row(row) => {
            elements(&row.cells)
                + row
                    .cells
                    .iter()
                    .map(|cell| match cell {
                        Cell::Text(text) => text.as_str().len(),
                        Cell::Number(_) | Cell::Bool(_) => 0,
                    })
                    .sum::<usize>()
        }
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
    use crate::{Finite, Record, parse_record, record::Point};

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
}
