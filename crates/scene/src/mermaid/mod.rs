//! Bounded Mermaid flowcharts, compiled into executable scene operations.
//!
//! The original step stays the record of what the model sent: duplicate checks, the log and
//! recovery all read it. Admission and preparation run its compiled form instead, where each
//! `mermaid` operation becomes, in place and in this order, a boxless root diagram inside its
//! target, the source's groups, its nodes in first-mention order and its edges in source order.
//! Group titles, reversed flows and link paint have no wire field, so they travel beside the
//! compiled step as private [`Extra`] facts keyed by executable slot.

mod parse;
mod read;

use crate::{
    Budget, ErrorCode, FieldPath, Step, StepError, Text, WireId,
    limits::{MAX_DIAGRAM_EDGES, MAX_DIAGRAM_NODES, MAX_STEP_OPS},
    record::{Diagram, Direction, Edge, Head, Node, Op},
    state::object::{self, Kind, Object},
    text::Bounded,
};
use parse::Flowchart;
use read::Stroke;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// A compiled slot's private drawing facts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Extra {
    /// A generated diagram's title, and whether its own `dir` runs backwards.
    Group { title: Option<Text>, reverse: bool },
    /// A compiled link's heads and whether it is drawn thick.
    Link { head: Head, thick: bool },
}
impl Extra {
    /// Segments a link charges: an arrow's curve with these heads, as many as it draws.
    pub(crate) fn segments(&self) -> Option<usize> {
        let Self::Link { head, .. } = self else {
            return None;
        };
        Some(
            crate::path::PROCEDURAL_CURVE_PIECES
                + match head {
                    Head::None => 0,
                    Head::Start | Head::End => 3,
                    Head::Both => 6,
                },
        )
    }
    /// Bytes these facts keep resident beside the prepared items.
    pub(crate) fn bytes(&self) -> usize {
        size_of::<(usize, Self)>()
            + match self {
                Self::Group {
                    title: Some(title), ..
                } => title.as_str().len(),
                _ => 0,
            }
    }
}

#[derive(Debug, Clone, Copy)]
enum Origin {
    Plain(usize),
    Root(usize),
    Source(usize),
}

/// A step ready for admission: the compiled operations and how to blame them.
pub(crate) struct Expansion {
    /// The step admission and preparation run; the original itself when it has no Mermaid.
    pub run: Arc<Step>,
    /// Private facts by executable slot.
    pub extras: Arc<BTreeMap<usize, Extra>>,
    /// Each executable slot's original operation; empty when nothing was compiled.
    origins: Vec<Origin>,
}

/// Compiles every `mermaid` operation in `original` for commit `revision`, with at most `live`
/// new objects. Generated ids skip every id the scene ever reserved and every id the step
/// declares or references, its sources' included, so they never alias a model's object or
/// capture a dangling reference.
/// # Errors
/// Fails at the source of the first Mermaid that is invalid, outside D1 or over a budget.
pub(crate) fn expand(
    original: &Arc<Step>,
    revision: u32,
    reserved: &BTreeSet<WireId>,
    live: usize,
) -> Result<Expansion, StepError> {
    let blocks = original
        .put
        .iter()
        .filter(|op| matches!(op, Op::Mermaid(_)))
        .count();
    if blocks == 0 {
        return Ok(Expansion {
            run: Arc::clone(original),
            extras: Arc::default(),
            origins: Vec::new(),
        });
    }
    let fail = |index, code| StepError::in_step(&original.id, source(index), code);
    let plain = original.put.len() - blocks;
    // Every compiled slot is an object, so both caps are known before any handle exists.
    let mut budget = live;
    let mut slots = MAX_STEP_OPS - plain;
    let mut charts = Vec::with_capacity(blocks);
    let referenced: Vec<WireId> = original
        .put
        .iter()
        .flat_map(object::reference_ids)
        .collect();
    let mut names: BTreeSet<&str> = referenced.iter().map(WireId::as_str).collect();
    names.extend(original.set.iter().map(|change| change.target.as_str()));
    names.extend(
        original
            .set
            .iter()
            .filter_map(|change| change.behind.as_ref().map(WireId::as_str)),
    );
    names.extend(original.remove.iter().map(WireId::as_str));
    for (index, op) in original.put.iter().enumerate() {
        if let Op::Mermaid(mermaid) = op {
            names.insert(mermaid.parent.as_str());
            let chart =
                parse::parse(mermaid.src.as_str(), budget).map_err(|code| fail(index, code))?;
            slots = slots
                .checked_sub(chart.objects())
                .ok_or_else(|| fail(index, ErrorCode::TooLong))?;
            budget -= chart.objects();
            names.extend(chart.nodes.iter().map(|node| node.id));
            names.extend(chart.groups.iter().filter_map(|group| group.id));
            charts.push(chart);
        } else if let Op::Anim(anim) = op {
            names.insert(anim.target.as_str());
        } else if let Some(visual) = object::visual(op) {
            names.insert(visual.id.as_str());
        }
    }
    let mut next = 0_usize;
    // Each skip passes one distinct taken id, so the search is bounded by the ids it avoids.
    let mut fresh = || loop {
        let id = WireId::try_from(format!("--{revision}.{next}"))
            .expect("a revision and a bounded counter fit an id");
        next += 1;
        if !reserved.contains(&id) && !names.contains(id.as_str()) {
            break id;
        }
    };
    let mut put = Vec::with_capacity(MAX_STEP_OPS - slots);
    let mut origins = Vec::new();
    let mut extras = BTreeMap::new();
    let mut charts = charts.into_iter();
    for (index, op) in original.put.iter().enumerate() {
        let Op::Mermaid(mermaid) = op else {
            put.push(op.clone());
            origins.push(Origin::Plain(index));
            continue;
        };
        let chart = charts
            .next()
            .expect("each Mermaid operation was parsed once");
        compile(&chart, &mermaid.parent, &mut fresh, &mut put, &mut extras)
            .map_err(|code| fail(index, code))?;
        origins.push(Origin::Root(index));
        origins.resize(put.len(), Origin::Source(index));
    }
    let run = Step {
        id: original.id.clone(),
        say: original.say.clone(),
        put: Bounded::new(put).expect("the parse budget bounds the compiled slots"),
        set: original.set.clone(),
        remove: original.remove.clone(),
        start: original.start.clone(),
    };
    Ok(Expansion {
        run: Arc::new(run),
        extras: Arc::new(extras),
        origins,
    })
}

impl Expansion {
    pub(crate) fn source_slot(&self, handle: crate::Handle, revision: u32) -> Option<usize> {
        (handle.revision() == revision
            && matches!(
                self.origins.get(handle.slot()),
                Some(Origin::Root(_) | Origin::Source(_))
            ))
        .then_some(handle.slot())
    }

    /// Moves a failure in the compiled step onto the original field that caused it.
    pub(crate) fn blame(&self, mut error: StepError) -> StepError {
        let Some((slot, tail)) = error.path.put_slot() else {
            return error;
        };
        let path = match self.origins.get(slot) {
            None => return error,
            Some(Origin::Plain(index)) => FieldPath::put_tail(*index, tail),
            Some(Origin::Root(index)) if tail == ".diagram.in" => mermaid(*index).field("in"),
            Some(Origin::Root(index) | Origin::Source(index)) => source(*index),
        };
        error.path = path;
        error
    }

    /// Requires a compiled root's target to be a diagram, where ordinary admission accepts any
    /// container.
    /// # Errors
    /// Returns `OutOfRange` at the root's `in`, which [`Self::blame`] maps to the Mermaid's.
    pub(crate) fn check_target(
        &self,
        run: &Step,
        slot: usize,
        objects: &BTreeMap<WireId, Object>,
    ) -> Result<(), StepError> {
        if let Some(Origin::Root(_)) = self.origins.get(slot)
            && let Op::Diagram(Diagram {
                parent: Some(target),
                ..
            }) = &run.put[slot]
            && objects
                .get(target)
                .is_some_and(|target| target.kind != Kind::Diagram)
        {
            return Err(StepError::in_step(
                &run.id,
                FieldPath::default()
                    .field("put")
                    .index(slot)
                    .field("diagram")
                    .field("in"),
                ErrorCode::OutOfRange,
            ));
        }
        Ok(())
    }
}

fn mermaid(index: usize) -> FieldPath {
    FieldPath::default()
        .field("put")
        .index(index)
        .field("mermaid")
}

fn source(index: usize) -> FieldPath {
    mermaid(index).field("src")
}

/// Appends one chart's operations and facts, first checking each diagram's own node and edge
/// caps where its final membership puts them.
#[expect(
    clippy::too_many_lines,
    reason = "check each diagram's caps, then emit its root, groups, nodes and edges in slot order"
)]
fn compile(
    chart: &Flowchart<'_>,
    target: &WireId,
    fresh: &mut impl FnMut() -> WireId,
    put: &mut Vec<Op>,
    extras: &mut BTreeMap<usize, Extra>,
) -> Result<(), ErrorCode> {
    let at = |group: Option<usize>| group.map_or(0, |group| group + 1);
    let mut counts = vec![[0_usize; 2]; chart.groups.len() + 1];
    for node in &chart.nodes {
        counts[at(node.owner)][0] += 1;
    }
    let common: Vec<_> = chart
        .edges
        .iter()
        .map(|edge| {
            lca(
                chart,
                chart.nodes[edge.from].owner,
                chart.nodes[edge.to].owner,
            )
        })
        .collect();
    for &group in &common {
        counts[at(group)][1] += 1;
    }
    for [nodes, edges] in counts {
        if nodes > MAX_DIAGRAM_NODES {
            return Err(ErrorCode::OverBudget(Budget::DiagramNodes));
        }
        if edges > MAX_DIAGRAM_EDGES {
            return Err(ErrorCode::OverBudget(Budget::DiagramEdges));
        }
    }
    let named = |id: &str| WireId::try_from(id.to_owned());
    let root = fresh();
    let groups = chart
        .groups
        .iter()
        .map(|group| group.id.map_or_else(|| Ok(fresh()), named))
        .collect::<Result<Vec<_>, _>>()?;
    let nodes = chart
        .nodes
        .iter()
        .map(|node| named(node.id))
        .collect::<Result<Vec<_>, _>>()?;
    let owner = |group: Option<usize>| group.map_or(&root, |group| &groups[group]).clone();
    let diagram = |id: WireId, parent: WireId, dir: Option<Direction>| {
        Op::Diagram(Diagram {
            id,
            parent: Some(parent),
            delay: None,
            at_word: None,
            enter: None,
            dur: None,
            color: None,
            opacity: None,
            bounds: None,
            dir,
        })
    };
    if chart.flow.reverse {
        extras.insert(
            put.len(),
            Extra::Group {
                title: None,
                reverse: true,
            },
        );
    }
    put.push(diagram(root.clone(), target.clone(), Some(chart.flow.dir)));
    for (group, id) in chart.groups.iter().zip(&groups) {
        let reverse = group.flow.is_some_and(|flow| flow.reverse);
        if group.title.is_some() || reverse {
            extras.insert(
                put.len(),
                Extra::Group {
                    title: group.title.clone(),
                    reverse,
                },
            );
        }
        put.push(diagram(
            id.clone(),
            owner(group.parent),
            group.flow.map(|flow| flow.dir),
        ));
    }
    for (node, id) in chart.nodes.iter().zip(&nodes) {
        put.push(Op::Node(Node {
            id: id.clone(),
            parent: Some(owner(node.owner)),
            delay: None,
            at_word: None,
            enter: None,
            dur: None,
            color: None,
            opacity: None,
            label: match &node.label {
                Some(label) => label.clone(),
                None => Text::try_from(node.id.to_owned())?,
            },
            sub: None,
            shape: node.shape,
        }));
    }
    for (edge, group) in chart.edges.iter().zip(common) {
        let thick = edge.stroke == Stroke::Thick;
        if thick || edge.head != Head::End {
            extras.insert(
                put.len(),
                Extra::Link {
                    head: edge.head,
                    thick,
                },
            );
        }
        put.push(Op::Edge(Edge {
            id: fresh(),
            parent: Some(owner(group)),
            delay: None,
            at_word: None,
            enter: None,
            dur: None,
            color: None,
            opacity: None,
            from: nodes[edge.from].clone(),
            to: nodes[edge.to].clone(),
            label: edge.label.clone(),
            dash: edge.stroke == Stroke::Dotted,
            flow: None,
        }));
    }
    Ok(())
}

/// The innermost group holding both groups, where the compiler ranks and routes their edge;
/// `None` is the chart's root.
fn lca(chart: &Flowchart<'_>, from: Option<usize>, to: Option<usize>) -> Option<usize> {
    let mut chain = Vec::new();
    let mut group = from;
    while let Some(index) = group {
        chain.push(index);
        group = chart.groups[index].parent;
    }
    let mut group = to;
    while let Some(index) = group {
        if chain.contains(&index) {
            return Some(index);
        }
        group = chart.groups[index].parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Extra, expand};
    use crate::{
        Budget, ErrorCode, FieldPath, Handle, Record, Step, StepError, WireId, parse_record,
        record::Op, state::object,
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };

    fn step(value: &Value) -> Arc<Step> {
        let Record::Step(step) =
            parse_record(&serde_json::to_vec(&json!({"step":value})).expect("JSON"))
                .expect("record")
        else {
            panic!("step");
        };
        Arc::new(step)
    }

    #[test]
    fn ordinary_steps_share_the_original_input_without_expansion() {
        let original = step(&json!({"id":"plain","put":[{"diagram":{"id":"d"}}]}));
        let result = expand(&original, 1, &BTreeSet::new(), 599).expect("plain input");
        assert!(Arc::ptr_eq(&original, &result.run));
        assert_eq!(result.extras.len(), 0);
    }

    #[test]
    fn generated_names_skip_historical_source_and_later_plain_ids() {
        let original = step(&json!({"id":"names","put":[
            {"diagram":{"id":"d"}},
            {"mermaid":{"in":"d","src":"flowchart TB\nsubgraph \"--1.1\"\nA\nend\n"}},
            {"shape":{"id":"--1.2","kind":"rect","box":[0,0,10,10]}}
        ]}));
        let reserved = BTreeSet::from([WireId::try_from("--1.0".to_owned()).expect("reserved id")]);
        let result = expand(&original, 1, &reserved, 598).expect("collision-free expansion");
        let ids: Vec<_> = result
            .run
            .put
            .iter()
            .filter_map(object::visual)
            .map(|visual| visual.id.as_str())
            .collect();
        assert_eq!(ids, ["d", "--1.3", "--1.1", "A", "--1.2"]);
        let error = StepError::in_step(
            &original.id,
            FieldPath::default()
                .field("put")
                .index(4)
                .field("shape")
                .field("id"),
            ErrorCode::IdReused,
        );
        assert_eq!(result.blame(error).path.to_string(), "put[2].shape.id");
    }

    #[test]
    fn generated_names_skip_ids_plain_operations_only_reference() {
        let original = step(&json!({"id":"refs","put":[
            {"diagram":{"id":"d"}},
            {"mermaid":{"in":"d","src":"flowchart TB\nA\n"}},
            {"text":{"id":"t","in":"--1.0","at":[0,0],"text":""}}
        ]}));
        let result = expand(&original, 1, &BTreeSet::new(), 598).expect("expanded source");
        let ids: Vec<_> = result
            .run
            .put
            .iter()
            .filter_map(object::visual)
            .map(|visual| (visual.id.as_str(), visual.parent.map(WireId::as_str)))
            .collect();
        assert_eq!(
            ids,
            [
                ("d", None),
                ("--1.1", Some("d")),
                ("A", Some("--1.1")),
                ("t", Some("--1.0")),
            ],
            "the dangling parent stays unresolved"
        );
    }

    #[test]
    fn every_reference_source_keeps_generated_roots_distinct() {
        for (parent, put, set, remove) in [
            (
                "d",
                json!([{"text":{"id":"t","in":"--1.0","at":[0,0],"text":""}}]),
                json!([]),
                json!([]),
            ),
            (
                "d",
                json!([{"text":{"id":"t","at":{"ref":"--1.0"},"text":""}}]),
                json!([]),
                json!([]),
            ),
            (
                "d",
                json!([{"anim":{"target":"--1.0","track":"opacity","keys":[[0,1]]}}]),
                json!([]),
                json!([]),
            ),
            (
                "d",
                json!([]),
                json!([{"target":"--1.0","color":"red"}]),
                json!([]),
            ),
            ("d", json!([]), json!([]), json!(["--1.0"])),
            ("--1.0", json!([]), json!([]), json!([])),
        ] {
            let mut value = json!({"id":"refs","put":[
                {"diagram":{"id":"d"}},
                {"mermaid":{"in":parent,"src":"flowchart TB\nA\n"}}
            ],"set":set,"remove":remove});
            value["put"].as_array_mut().expect("put array").extend(
                put.as_array()
                    .expect("reference operations")
                    .iter()
                    .cloned(),
            );
            let result =
                expand(&step(&value), 1, &BTreeSet::new(), 598).expect("expanded references");
            let Op::Diagram(root) = &result.run.put[1] else {
                panic!("compiled root");
            };
            assert_eq!(root.id.as_str(), "--1.1");
            assert_eq!(root.parent.as_ref().map(WireId::as_str), Some(parent));
        }
    }

    #[test]
    fn one_diagram_holds_96_edges_and_refuses_97() {
        let chart = |edges: usize| {
            let src = format!("flowchart LR\n{}", "A --> B\n".repeat(edges));
            step(&json!({"id":"edges","put":[
                {"diagram":{"id":"d"}},
                {"mermaid":{"in":"d","src":src}}
            ]}))
        };
        let result = expand(&chart(96), 1, &BTreeSet::new(), 599).expect("96 edges");
        assert_eq!(
            result.run.put.len(),
            100,
            "d, the root, two nodes and 96 edges"
        );
        let error = expand(&chart(97), 1, &BTreeSet::new(), 599)
            .err()
            .expect("97 edges");
        assert_eq!(
            (error.code, error.path.to_string()),
            (
                ErrorCode::OverBudget(Budget::DiagramEdges),
                "put[1].mermaid.src".to_owned()
            )
        );
    }

    #[test]
    fn private_metadata_and_generated_diagnostics_keep_compiled_slots() {
        let original = step(&json!({"id":"metadata","put":[
            {"table":{"id":"d","cols":["C"],"box":[0,0,100,40]}},
            {"mermaid":{"in":"d","src":"flowchart LR\nsubgraph g[Group]\ndirection BT\nA <==>|join| B\nend\n"}}
        ]}));
        let result = expand(&original, 1, &BTreeSet::new(), 599).expect("expanded source");
        assert_eq!(result.run.put.len(), 6);
        let Some(Extra::Group { title, reverse }) = result.extras.get(&2) else {
            panic!("group facts");
        };
        assert_eq!(
            (title.as_ref().map(crate::Text::as_str), *reverse),
            (Some("Group"), true)
        );
        assert_eq!(result.extras[&5].segments(), Some(22));
        assert_eq!(result.extras[&2].segments(), None);
        let no_title = Extra::Group {
            title: None,
            reverse: true,
        };
        assert_eq!(result.extras[&2].bytes() - no_title.bytes(), 5);
        let generated = Handle::new(1, 2).expect("generated handle");
        assert_eq!(result.source_slot(generated, 1), Some(2));
        assert_eq!(result.source_slot(generated, 2), None);
        assert_eq!(
            result.source_slot(Handle::new(1, 0).expect("plain handle"), 1),
            None
        );
        let error = StepError::in_step(
            &original.id,
            FieldPath::default().field("put").index(2),
            ErrorCode::TooDeep,
        );
        assert_eq!(result.blame(error).path.to_string(), "put[1].mermaid.src");
        let op = &result.run.put[0];
        assert!(matches!(op, Op::Table(_)));
        let visual = object::visual(op).expect("table visual");
        let target = object::Object {
            handle: Handle::new(1, 0).expect("target handle"),
            kind: object::Kind::Table,
            references: Box::default(),
            parent: None,
            work: crate::state::work::Work::default(),
            three: None,
        };
        let objects = BTreeMap::from([(visual.id.clone(), target)]);
        let error = result
            .check_target(&result.run, 1, &objects)
            .expect_err("table isn't a diagram");
        let error = result.blame(error);
        assert_eq!(
            (error.code, error.path.to_string()),
            (ErrorCode::OutOfRange, "put[1].mermaid.in".to_owned())
        );
    }
}
