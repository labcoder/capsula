//! Requested heap bytes scene values hold, pinned to Rust 1.98's collection layouts.

use super::state::{Motions, Placed};
use crate::{
    Op, Step, Text, WireId,
    mermaid::Extra,
    record::{Anchor, Axis, Cell, Change, Start, Target},
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
// shortcut: std's node layout is private; btree_node_allowance_bounds_requested_allocations
// rechecks the bound on each toolchain bump.
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

/// Bytes of source copies and motion maps that earlier prefixes don't already hold.
pub(super) fn shared_bytes<'a>(
    shared: impl IntoIterator<Item = &'a Placed>,
    objects: impl IntoIterator<Item = &'a Placed>,
) -> usize {
    let mut sources = HashSet::new();
    let mut motions = HashSet::new();
    for object in shared {
        sources.insert(Arc::as_ptr(&object.source));
        if let Some(motion) = &object.motion {
            motions.insert(Arc::as_ptr(motion));
        }
    }
    let mut bytes = 0;
    for object in objects {
        if sources.insert(Arc::as_ptr(&object.source)) {
            bytes += ARC_HEADER + size_of::<Op>() + op_bytes(&object.source);
        }
        if let Some(motion) = &object.motion
            && motions.insert(Arc::as_ptr(motion))
        {
            bytes += ARC_HEADER
                + size_of::<Motions>()
                + btree::<&'static str, crate::state::work::Motion>(motion.len());
        }
    }
    bytes
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
        + optional(change.info.as_ref())
        + change.behind.as_ref().map_or(0, |id| id.as_str().len())
        + change.cells.as_deref().map_or(0, cells)
        + change.data.as_deref().map_or(0, elements)
}

/// The compiled record a prepared ticket owns, unless it shares the original log record.
pub(crate) fn compiled_bytes(original: &Arc<Step>, run: &Arc<Step>) -> usize {
    if Arc::ptr_eq(original, run) {
        return 0;
    }
    ARC_HEADER + step_bytes(run)
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
/// One shared record copy with its `Arc` header.
pub(crate) fn source_bytes(op: &Op) -> usize {
    ARC_HEADER + size_of::<Op>() + op_bytes(op)
}

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
            target(&callout.target)
                + callout.at.as_ref().map_or(0, anchor)
                + callout.text.as_str().len()
        }
        Op::Highlight(highlight) => target(&highlight.target),
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
        Op::View3d(view) => view.id.as_str().len(),
        Op::Solid(solid) => {
            solid.id.as_str().len()
                + solid.parent.as_str().len()
                + solid.attach.as_ref().map_or(0, |id| id.as_str().len())
                + optional(solid.label.as_ref())
                + optional(solid.info.as_ref())
                + elements(&solid.points)
                + elements(&solid.path)
        }
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

fn target(value: &Target) -> usize {
    match value {
        Target::Anchor(value) => anchor(value),
        Target::Box(_) => 0,
    }
}

#[cfg(test)]
#[path = "../../tests/support/allocation.rs"]
mod allocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preparation_caches_only_its_source_and_sizing_allocations() {
        let Record::Step(step) =
            parse_record(br#"{"step":{"id":"s","put":[{"path":{"id":"p","d":"M0 0L1 1"}}]}}"#)
                .expect("path step")
        else {
            panic!("step")
        };
        let (source, _, [allocated, released]) = allocation::usage(|| Arc::new(step.clone()));
        let source_bytes = allocated - released;
        let Record::Begin(begin) =
            parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin")
        };
        let mut scene = crate::SceneState::begin(begin).expect("scene");
        let mut budget = crate::GlobalBudget::default();
        let crate::Admission::New(candidate) = scene.admit(&step, &budget).expect("admission")
        else {
            panic!("new step")
        };
        let ticket = scene
            .commit(candidate, &mut budget)
            .expect("commit")
            .preparation;
        assert!(Arc::ptr_eq(&ticket.step, &ticket.run));
        assert_eq!(ticket.path_scales.len(), 1);
        assert_eq!(ticket.extras.len(), 0);
        assert_eq!(
            ticket.attributed_bytes(),
            source_bytes + 16 + 40 + 104,
            "source allocation, owner Arc, empty extras Arc/map and one 104-byte B-tree leaf"
        );
        drop(source);
    }
    use crate::{
        Finite, Record, WireId, parse_record,
        record::{Change, Point},
        state::{
            object::{Kind, Object},
            work::Work,
        },
    };

    #[derive(Debug, Clone, Copy)]
    enum Construction {
        Insert,
        Collect,
        /// Bulk-builds every entry, then removes from the front down to this many.
        Shrink(usize),
    }

    fn check_nodes<K: Ord, V>(mut entries: Vec<(K, V)>, construction: Construction) {
        let len = match construction {
            Construction::Shrink(keep) => keep,
            Construction::Insert | Construction::Collect => entries.len(),
        };
        let mut removed = Vec::with_capacity(entries.len());
        let (map, _, [allocated, released]) = allocation::usage(|| match construction {
            Construction::Collect => entries.drain(..).collect::<BTreeMap<_, _>>(),
            Construction::Insert => {
                let mut map = BTreeMap::new();
                for (key, value) in entries.drain(..) {
                    map.insert(key, value);
                }
                map
            }
            Construction::Shrink(keep) => {
                let mut map = entries.drain(..).collect::<BTreeMap<_, _>>();
                while map.len() > keep {
                    removed.extend(map.pop_first());
                }
                map
            }
        });
        assert_eq!(map.len(), len, "construction retains the fixture's entries");
        let held = allocated
            .checked_sub(released)
            .expect("only measured node storage remains");
        if len == 0 && matches!(construction, Construction::Shrink(_)) {
            assert_eq!(
                held,
                btree::<K, V>(1),
                "a map that removals emptied keeps one uncharged root leaf"
            );
            return;
        }
        let allowance = btree::<K, V>(len);
        assert!(
            held <= allowance,
            "{len} entries, {construction:?}: {held} requested node bytes exceed {allowance}"
        );
        if len <= 10 {
            assert_eq!(
                held, allowance,
                "an empty map or one root leaf has an exact charge"
            );
        }
    }

    /// Grown trees at each root boundary, then trees removals shrank back to them.
    fn check_shapes<K: Ord, V>(entry: impl Fn(usize) -> (K, V)) {
        for len in [0, 1, 10, 11, 12, 100, 256] {
            for construction in [Construction::Insert, Construction::Collect] {
                check_nodes((0..len).map(&entry).collect(), construction);
            }
        }
        for keep in [0, 1, 10, 11, 100] {
            check_nodes((0..256).map(&entry).collect(), Construction::Shrink(keep));
        }
    }

    #[test]
    fn btree_node_allowance_bounds_requested_allocations() {
        let (empty, allocations) = allocation::count(BTreeMap::<usize, usize>::new);
        assert_eq!(
            (empty.len(), allocations),
            (0, 0),
            "a fresh empty map allocates nothing"
        );
        let id = |i: usize| -> WireId { format!("r{i}").try_into().expect("bounded id") };
        let handle = |i| crate::Handle::new(1, i).expect("bounded handle");
        check_shapes(|i| (i, i));
        check_shapes(|i| {
            let object = Object {
                work: Work::default(),
                handle: handle(i),
                kind: Kind::Shape,
                parent: None,
                references: Box::default(),
                three: None,
            };
            (id(i), object)
        });
        let source = Arc::new(op(r#"{"shape":{"id":"s","kind":"rect","box":[0,0,1,1]}}"#));
        let primitives: Arc<[crate::display::Prim]> = Arc::new([]);
        check_shapes(|i| {
            let item = crate::display::Item {
                handle: handle(i),
                parent: None,
                primitives: Arc::clone(&primitives),
                bounds: crate::geom::Rect::default(),
                transform: crate::geom::Affine::default(),
                opacity: 1.0,
            };
            (id(i), Placed::new(item, Arc::clone(&source), None))
        });
        check_shapes(|i| (handle(i), id(i)));
        let facts: Arc<BTreeMap<usize, Extra>> = Arc::new(BTreeMap::new());
        check_shapes(|i| (u32::try_from(i).expect("small key"), Arc::clone(&facts)));
        check_shapes(|i| {
            let link = Extra::Link {
                head: crate::record::Head::Both,
                thick: true,
            };
            (i, link)
        });
        let names: Vec<String> = (0..256).map(|i| format!("p{i}")).collect();
        let motion = crate::state::work::Motion {
            keys: 2,
            min: [0.0; 2],
            max: [1.0; 2],
        };
        check_shapes(|i| (names[i].as_str(), motion));
    }

    fn op(json: &str) -> Op {
        let record = format!(r#"{{"step":{{"id":"s","put":[{json}]}}}}"#);
        let Record::Step(step) = parse_record(record.as_bytes()).expect("typed step") else {
            panic!("step");
        };
        step.put[0].clone()
    }

    #[test]
    fn sources_charge_owned_sequence_elements_at_their_own_size() {
        assert_eq!(op_bytes(&op(r#"{"view3d":{"id":"view"}}"#)), 4);
        let solid = op(
            r#"{"solid":{"id":"solid","in":"view","shape":"box","points":[[1,2]],"path":[[1,2,3]],"attach":"anchor","label":"label","info":"info"}}"#,
        );
        assert_eq!(
            op_bytes(&solid),
            24 + size_of::<[Finite; 2]>() + size_of::<[Finite; 3]>()
        );
        let Record::Step(step) = parse_record(br#"{"step":{"id":"s","put":[{"view3d":{"id":"view"}},{"solid":{"id":"solid","in":"view","shape":"box","points":[[1,2]],"path":[[1,2,3]],"attach":"anchor","label":"label","info":"info"}}]}}"#).expect("3D step") else { panic!("step") };
        let original = Arc::new(step);
        let (copied, _, [allocated, released]) =
            allocation::usage(|| Arc::new((*original).clone()));
        assert_eq!(compiled_bytes(&original, &copied), allocated - released);
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
        let Record::Step(record) = parse_record(br#"{"step":{"id":"s","say":"say","put":[{"shape":{"id":"p","kind":"rect","box":[0,0,1,1]}}],"set":[{"target":"p","text":"tt","label":"lll","info":"\u00e9\u00bd\u4e2d","cells":["cc",1],"data":[1,2]}],"remove":["zz"],"start":{"after":"before","ms":0}}}"#).expect("typed record") else { panic!("step"); };
        let original = Arc::new(record);
        assert_eq!(compiled_bytes(&original, &original), 0);
        let (copied, _, [allocated, released]) =
            allocation::usage(|| Arc::new((*original).clone()));
        assert_eq!(compiled_bytes(&original, &copied), allocated - released);
        assert_eq!(
            compiled_bytes(&original, &copied),
            ARC_HEADER
                + size_of::<Step>()
                + size_of::<Op>()
                + size_of::<Change>()
                + size_of::<WireId>()
                + 2 * size_of::<Cell>()
                + 2 * size_of::<Point>()
                + 28
        );
    }

    #[test]
    fn private_facts_count_shared_allocations_once_with_their_headers() {
        let (facts, _, [allocated, released]) = allocation::usage(|| {
            Arc::new(BTreeMap::from([
                (
                    0,
                    Extra::Group {
                        title: Some("title".to_owned().try_into().expect("title")),
                        reverse: true,
                    },
                ),
                (
                    1,
                    Extra::Link {
                        head: crate::record::Head::Both,
                        thick: true,
                    },
                ),
            ]))
        });
        let charge = extras_bytes(std::iter::empty(), [&facts, &facts]);
        assert_eq!(charge, allocated - released);
        assert_eq!(extras_bytes([&facts], [&facts, &facts]), 0);
        let empty = Arc::new(BTreeMap::new());
        assert_eq!(
            extras_bytes(std::iter::empty(), [&empty, &empty]),
            ARC_HEADER + size_of::<BTreeMap<usize, Extra>>()
        );
    }

    #[test]
    fn expanded_run_and_private_facts_bound_the_compilers_retained_allocations() {
        let Record::Step(record) = parse_record(br#"{"step":{"id":"s","say":"words","put":[{"diagram":{"id":"d"}},{"mermaid":{"in":"d","src":"flowchart LR\nsubgraph g[Group]\ndirection BT\nA <==>|join| B\nend\n"}}],"start":{"after":"before","ms":0}}}"#).expect("typed record") else { panic!("step"); };
        let original = Arc::new(record);
        let ((run, facts), _, [allocated, released]) = allocation::usage(|| {
            let expanded =
                crate::mermaid::expand(&original, 1, &std::collections::BTreeSet::new(), 600)
                    .expect("bounded flowchart");
            (expanded.run, expanded.extras)
        });
        assert!(!Arc::ptr_eq(&original, &run));
        assert_eq!(facts.len(), 2);
        let charge = compiled_bytes(&original, &run) + extras_bytes(std::iter::empty(), [&facts]);
        assert!(
            allocated - released <= charge,
            "compiled run and facts hold {} bytes but charge {charge}",
            allocated - released
        );
    }
}
