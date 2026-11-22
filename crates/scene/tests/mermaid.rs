//! Bounded Mermaid compilation through admission, preparation and original-log recovery.
use kumochi_scene::{
    Admission, Budget, ErrorCode, Frame, GlobalBudget, Handle, SceneState, SceneView, Step,
    StopCut,
    display::{Item, Prim},
    geom::{Affine, Rect, Vec2},
    layout::{LayoutState, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover},
    record::Align,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    sync::{Arc, atomic::AtomicBool},
};

#[path = "support/mermaid.rs"]
mod mermaid;
use mermaid::{connected_full_graph, graph, scene, typed};

#[path = "support/scene.rs"]
#[expect(dead_code, reason = "typed Mermaid fixtures admit Steps directly")]
mod scene_support;
use scene_support::{Measure, id, new_scene};

#[path = "support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared allocator has helpers this file doesn't use"
)]
mod allocation;

fn picture(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    step: &Step,
) -> PreparedStep {
    let Admission::New(candidate) = scene.admit(step, budget).expect("admitted graph") else {
        panic!("new graph step");
    };
    let committed = scene.commit(candidate, budget).expect("fresh candidate");
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("consecutive picture");
    let prepared = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("graph picture");
    prepared.validate(scene, before).expect("current picture");
    prepared
}

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("safe bounds")
        .expect("node")
}

fn labelled<'a>(items: &'a [Item], label: &str) -> &'a Item {
    items
        .iter()
        .find(|item| {
            item.primitives
                .iter()
                .any(|prim| matches!(prim, Prim::Text { text, .. } if text.as_str() == label))
        })
        .expect("labelled object")
}

fn assert_rect(actual: Rect, expected: [f32; 4]) {
    assert_eq!(
        [
            actual.origin.x,
            actual.origin.y,
            actual.size.x,
            actual.size.y
        ]
        .map(f32::to_bits),
        expected.map(f32::to_bits),
        "the world rectangle has its source-derived position and size"
    );
}

fn title_origin(layout: &LayoutState, label: &str) -> Vec2 {
    let items = layout.items();
    let item = labelled(&items, label);
    let origin = item
        .primitives
        .iter()
        .find_map(|primitive| match primitive {
            Prim::Text { text, origin, .. } if text.as_str() == label => Some(*origin),
            _ => None,
        })
        .expect("title text");
    world_transform(&items, item).point(origin)
}

fn world_transform(items: &[Item], item: &Item) -> Affine {
    let mut transform = item.transform;
    let mut parent = item.parent;
    while let Some(handle) = parent {
        let ancestor = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("title ancestor");
        transform = ancestor.transform.compose(transform);
        parent = ancestor.parent;
    }
    transform
}

fn at_rest(layout: &LayoutState) -> Frame {
    let mut frame = Frame::new();
    SceneView::new(layout, 0)
        .frame(0, &mut frame)
        .expect("paint at rest");
    frame
}

fn stroke(primitives: &[Prim]) -> Option<(&[Vec2], f32, bool)> {
    primitives.iter().find_map(|prim| match prim {
        Prim::Line {
            points,
            width,
            dashed,
            closed: false,
            ..
        } => Some((&**points, *width, *dashed)),
        _ => None,
    })
}

fn near(p: Vec2, q: Vec2) -> bool {
    (p.x - q.x).hypot(p.y - q.y) < 0.01
}

fn edge_handle(layout: &LayoutState, frame: &Frame, from: &str, to: &str) -> Handle {
    let a = bounds(layout, from);
    let b = bounds(layout, to);
    let start = a.edge_toward(b.center());
    let end = b.edge_toward(a.center());
    let mut matched = frame.paint_items().filter_map(|(item, _, primitives)| {
        let (points, _, _) = stroke(primitives)?;
        (near(item.transform.point(*points.first()?), start)
            && near(item.transform.point(*points.last()?), end))
        .then_some(item.handle)
    });
    let handle = matched
        .next()
        .unwrap_or_else(|| panic!("no rendered stroke for {from} -> {to}"));
    assert_eq!(
        matched.next(),
        None,
        "fixture has one {from} -> {to} stroke"
    );
    handle
}

fn rejected(scene: &SceneState, budget: &GlobalBudget, step: &Step, code: ErrorCode, path: &str) {
    let before = (
        scene.revision(),
        scene.object_count(),
        scene.usage(),
        scene.recovery_log().len(),
        budget.usage(),
        budget.generation(),
    );
    let error = scene.admit(step, budget).expect_err("rejected source");
    assert_eq!(
        error.code, code,
        "the source has its specified refusal code"
    );
    assert_eq!(
        error.path.to_string(),
        path,
        "the error names the original source field"
    );
    assert_eq!(
        (
            scene.revision(),
            scene.object_count(),
            scene.usage(),
            scene.recovery_log().len(),
            budget.usage(),
            budget.generation()
        ),
        before,
        "failed compilation/admission is atomic"
    );
}

#[test]
fn approved_gates_prepare_and_recover_from_original_source() {
    for (source, count) in [
        (include_str!("fixtures/mermaid/gates/branching.mmd"), 14),
        (
            include_str!("fixtures/mermaid/gates/long-label-cycle.mmd"),
            12,
        ),
        (
            include_str!("fixtures/mermaid/gates/nested-cross-group.mmd"),
            16,
        ),
        (include_str!("fixtures/mermaid/gates/orientations.mmd"), 10),
        (include_str!("fixtures/mermaid/gates/two-way.mmd"), 6),
        (include_str!("fixtures/mermaid/gates/self-loop.mmd"), 4),
        (
            include_str!("fixtures/mermaid/gates/labelled-edges.mmd"),
            12,
        ),
    ] {
        let mut scene = scene();
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let step = graph(source);
        let prepared = picture(&mut scene, &mut budget, &before, &step);
        assert_eq!(scene.step(0), Some(&step));
        assert_eq!(prepared.step(), &step);
        assert_eq!(scene.object_count(), count);
        let (replayed, recovered) = recover(
            scene.header().clone(),
            scene.recovery_log(),
            SurfaceGeometry::default(),
            &Arc::new(Theme::default()),
            &mut Measure,
            &AtomicBool::new(false),
            None,
        )
        .expect("original graph recovery");
        assert_eq!(replayed.step(0), Some(&step));
        assert_eq!(replayed.object_count(), count);
        assert_eq!(recovered.items(), prepared.after.items());
    }
}

#[test]
fn cardinal_directions_change_the_actual_node_order() {
    let mut tb_gap = None;
    for (direction, horizontal, reverse) in [
        ("LR", true, false),
        ("RL", true, true),
        ("TB", false, false),
        ("TD", false, false),
        ("BT", false, true),
    ] {
        let mut scene = scene();
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let prepared = picture(
            &mut scene,
            &mut budget,
            &before,
            &graph(&format!("flowchart {direction}\nA --> B\n")),
        );
        let (a, b) = (bounds(&prepared.after, "A"), bounds(&prepared.after, "B"));
        let (leading, trailing) = if reverse { (b, a) } else { (a, b) };
        let gap = if horizontal {
            assert_eq!(a.center().y.to_bits(), b.center().y.to_bits());
            trailing.origin.x - leading.max().x
        } else {
            assert_eq!(a.center().x.to_bits(), b.center().x.to_bits());
            trailing.origin.y - leading.max().y
        };
        assert!(gap > 0.0, "{direction} separates the authored ranks");
        if direction == "TB" {
            tb_gap = Some(gap.to_bits());
        }
        if direction == "TD" {
            assert_eq!(Some(gap.to_bits()), tb_gap);
        }
    }
}

#[test]
fn root_references_do_not_override_first_closed_group_membership() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph(
            "flowchart TB\nsubgraph first[First]\nA[One]\nend\nsubgraph second[Second]\nB[Two]\nend\nA -->|join| B\n",
        ),
    );
    let items = prepared.after.items();
    for (node, group) in [("A", "first"), ("B", "second")] {
        let handle = scene.handle(&id(node)).expect("compiled node");
        let item = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("node item");
        assert_eq!(
            item.parent,
            Some(scene.handle(&id(group)).expect("authored group"))
        );
    }
    let target = scene.handle(&id("target")).expect("authored diagram");
    let groups = ["first", "second"].map(|name| scene.handle(&id(name)));
    let mut parent = labelled(&items, "join").parent;
    for _ in 0..items.len() {
        if parent == Some(target) {
            break;
        }
        assert!(!groups.contains(&parent), "the join is outside both groups");
        let ancestor = parent.expect("the join stays inside its authored diagram");
        parent = items
            .iter()
            .find(|item| item.handle == ancestor)
            .expect("ancestor")
            .parent;
    }
    assert_eq!(parent, Some(target));
}

#[test]
fn an_earlier_closed_reference_keeps_membership_before_a_later_declaration() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph(
            "flowchart TB\nsubgraph first[First]\nA --> B\nend\nsubgraph second[Second]\nA[Named later]\nC\nend\n",
        ),
    );
    let items = prepared.after.items();
    let node = labelled(&items, "Named later");
    assert_eq!(node.handle, scene.handle(&id("A")).expect("A"));
    assert_eq!(node.parent, scene.handle(&id("first")));
}

#[test]
fn refused_families_never_commit_a_valid_prefix() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/mermaid/refused.json"))
        .expect("refused fixtures");
    let cases = cases.as_array().expect("fixture array").iter().map(|case| {
        let source = case["src"].as_str().expect("source");
        let expected = match case["code"].as_str().expect("diagnostic code") {
            "Malformed" => ErrorCode::Malformed,
            "Unsupported" => ErrorCode::Unsupported,
            code => panic!("unknown fixture code: {code}"),
        };
        (source, expected)
    });
    for (source, expected) in cases.chain([
        (
            "graph TD;A_node-->odd->Vertex Text];",
            ErrorCode::Unsupported,
        ),
        (
            "flowchart TB\nA --> B[\"test\"]:::hot\n",
            ErrorCode::Unsupported,
        ),
    ]) {
        let scene = scene();
        let budget = GlobalBudget::default();
        rejected(
            &scene,
            &budget,
            &graph(source),
            expected,
            "put[1].mermaid.src",
        );
        assert_eq!(scene.object_count(), 0);
        assert_eq!(scene.revision(), 0);
        assert_eq!(budget.usage().objects, 0);
    }
}

#[test]
fn centered_node_and_edge_labels_start_their_full_wrapping_box_at_the_same_spine() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart TB\nA[Start] -->|next| B[Done]\n"),
    );
    let spine = bounds(&prepared.after, "A").center().x;
    assert_eq!(
        spine.to_bits(),
        bounds(&prepared.after, "B").center().x.to_bits()
    );
    let frame = at_rest(&prepared.after);
    for text in ["Start", "next", "Done"] {
        let mut labels = frame.paint_items().flat_map(|(item, _, prims)| prims.iter().map(move |p| (item, p)))
            .filter(|(_, prim)| matches!(prim, Prim::Text { text: value, .. } if value.as_str() == text));
        let Some((
            item,
            Prim::Text {
                origin,
                max_width: Some(width),
                align: Align::Center,
                ..
            },
        )) = labels.next()
        else {
            panic!("{text} must have a centered wrapping box");
        };
        assert_eq!(width.to_bits(), 180.0_f32.to_bits());
        assert_eq!(
            item.transform
                .point(*origin + Vec2::new(*width * 0.5, 0.0))
                .x
                .to_bits(),
            spine.to_bits()
        );
        assert!(labels.next().is_none(), "one label for {text}");
    }
}

#[test]
fn a_header_only_chart_keeps_one_empty_boxless_root() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(&mut scene, &mut budget, &before, &graph("flowchart LR"));
    assert_eq!(scene.object_count(), 2);
    assert_eq!(budget.usage().objects, 2);
    assert!(at_rest(&prepared.after).paint_items().next().is_none());
}

#[test]
fn explicit_blank_labels_stay_empty_on_normal_nodes() {
    for source in ["flowchart LR\nA[\" \"]", "flowchart LR\nA[ ]"] {
        let mut scene = scene();
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let prepared = picture(&mut scene, &mut budget, &before, &graph(source));
        let node = scene.handle(&id("A")).expect("named node");
        let frame = at_rest(&prepared.after);
        let mut paint = frame
            .paint_items()
            .filter(|(item, _, _)| item.handle == node)
            .flat_map(|(_, _, prims)| prims)
            .peekable();
        assert!(
            paint.peek().is_some(),
            "node A paints even with a blank label"
        );
        let mut has_outline = false;
        for prim in paint {
            has_outline |= matches!(
                prim,
                Prim::Rect {
                    border: Some(_),
                    ..
                }
            );
            assert!(!matches!(prim, Prim::Text { text, .. } if !text.as_str().is_empty()));
        }
        assert!(has_outline);
        assert_eq!(scene.object_count(), 3);
        assert_eq!(budget.usage().objects, 3);
    }
}

#[test]
fn keywords_and_plain_comparison_text_are_ordinary_labels() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph("flowchart LR\nA[\"click style direction end: 1 < 2\"] -->|class import| B[Done]\n"),
    );
    let items = prepared.after.items();
    assert_eq!(
        labelled(&items, "click style direction end: 1 < 2").handle,
        scene.handle(&id("A")).expect("A")
    );
    assert_eq!(
        labelled(&items, "class import").handle,
        edge_handle(&prepared.after, &at_rest(&prepared.after), "A", "B")
    );
}

#[test]
fn compiled_edges_keep_distinct_heads_strokes_and_labels() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph(include_str!("fixtures/mermaid/gates/labelled-edges.mmd")),
    );
    let items = prepared.after.items();
    let mut normal_width = 0.0;
    let mut thick_width = 0.0;
    let frame = at_rest(&prepared.after);
    for (label, from, to, heads, dotted) in [
        ("normal", "request", "process", [0, 1], false),
        ("retry", "process", "retry", [0, 1], true),
        ("batch", "retry", "result", [0, 1], false),
        ("shared", "request", "result", [0, 0], false),
        ("sync", "result", "store", [1, 1], false),
    ] {
        let item = labelled(&items, label);
        assert_eq!(item.handle, edge_handle(&prepared.after, &frame, from, to));
        let (points, width, dashed) = stroke(&item.primitives).expect("edge stroke");
        let end = points.len() - 1;
        let actual_heads =
            [(points[0], points[1]), (points[end], points[end - 1])].map(|(tip, inside)| {
                let outward = tip - inside;
                let projection = |point: Vec2| {
                    let delta = point - tip;
                    delta.x * outward.x + delta.y * outward.y
                };
                item.primitives
                    .iter()
                    .filter(|prim| {
                        matches!(prim, Prim::Fill { points, .. }
                    if points.iter().any(|&p| near(p, tip))
                        && points.iter().any(|&p| projection(p) < 0.0)
                        && points.iter().all(|&p| projection(p) <= 0.01))
                    })
                    .count()
            });
        assert_eq!(actual_heads, heads, "edge {label} start/end heads");
        assert_eq!(dashed, dotted, "edge {label}");
        if label == "normal" {
            normal_width = width;
        }
        if label == "batch" {
            thick_width = width;
        }
    }
    assert!(normal_width > 0.0 && thick_width > normal_width);
}

#[test]
fn node_and_edge_limits_are_per_diagram_not_per_source() {
    let mut source = String::from("flowchart TB\n");
    for group in ["left", "right"] {
        writeln!(source, "subgraph {group}").expect("source string");
        for node in 0..25 {
            writeln!(source, "{group}{node}[N]").expect("source string");
        }
        for edge in 0..50 {
            writeln!(
                source,
                "{group}{} -->|{group}-edge-{edge}| {group}{}",
                edge % 25,
                (edge + 1) % 25
            )
            .expect("source string");
        }
        source.push_str("end\n");
    }
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(&mut scene, &mut budget, &before, &graph(&source));
    let handles: std::collections::BTreeSet<_> = ["left", "right"]
        .into_iter()
        .flat_map(|group| (0..25).map(move |node| id(&format!("{group}{node}"))))
        .map(|id| prepared.after.handle(&id).expect("authored node"))
        .collect();
    assert_eq!(handles.len(), 50);
    let items = prepared.after.items();
    for group in ["left", "right"] {
        for edge in 0..50 {
            labelled(&items, &format!("{group}-edge-{edge}"));
        }
    }
    let mut single = String::from("flowchart TB\n");
    for node in 0..49 {
        writeln!(single, "n{node}[N]").expect("source string");
    }
    let empty = SceneState::begin(scene.header().clone()).expect("fresh scene");
    let error = empty
        .admit(&graph(&single), &GlobalBudget::default())
        .expect_err("49 nodes in one group");
    assert_eq!(error.code, ErrorCode::OverBudget(Budget::DiagramNodes));
}

#[test]
fn duplicate_identity_uses_the_original_step_not_normalized_graphs() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let step = graph("flowchart LR\nA --> B\n");
    picture(&mut scene, &mut budget, &before, &step);
    let usage = budget.usage();
    assert!(matches!(
        scene.admit(&step, &budget),
        Ok(Admission::Duplicate)
    ));
    assert_eq!(scene.step(0), Some(&step));
    assert_eq!(budget.usage(), usage);
    let changed = graph("flowchart LR; A --> B;");
    assert_eq!(
        scene
            .admit(&changed, &budget)
            .expect_err("different original source")
            .code,
        ErrorCode::DuplicateStepConflict
    );
}

#[test]
fn group_labels_and_edge_heads_survive_set_cut_and_recovery() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph("flowchart RL\nsubgraph cluster[Named group]\nA -->|join| B\nend\n"),
    );
    let second = picture(
        &mut scene,
        &mut budget,
        &first.after,
        &typed(&json!({"id":"recolor","set":[{"target":"cluster","color":"#abcdef"}]})),
    );
    let items = second.after.items();
    let cluster = scene.handle(&id("cluster")).expect("cluster");
    assert_eq!(labelled(&items, "Named group").handle, cluster);
    let edge = labelled(&items, "join").handle;
    let expected_a = bounds(&second.after, "A");
    let expected_b = bounds(&second.after, "B");
    let revealed = [
        cluster,
        scene.handle(&id("A")).expect("A"),
        scene.handle(&id("B")).expect("B"),
        edge,
    ];
    scene
        .stop(
            &StopCut {
                activated: 2,
                revealed: Bounded::new(revealed.to_vec()).expect("bounded cut"),
            },
            &mut budget,
        )
        .expect("cut");
    let detached = second.after.cut(&scene).expect("detached graph");
    let (_, recovered) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("cut graph recovery");
    for layout in [&detached, &recovered] {
        assert_eq!(bounds(layout, "A"), expected_a);
        assert_eq!(bounds(layout, "B"), expected_b);
        let items = layout.items();
        assert_eq!(labelled(&items, "Named group").handle, cluster);
        let join = labelled(&items, "join");
        assert_eq!(join.handle, edge);
        let tip = join
            .primitives
            .iter()
            .find_map(|prim| match prim {
                Prim::Line {
                    points,
                    closed: false,
                    ..
                } => points.last(),
                _ => None,
            })
            .expect("the join shaft");
        assert!(
            join.primitives
                .iter()
                .any(|prim| { matches!(prim, Prim::Fill { points, .. } if points.contains(tip)) }),
            "the join has a filled head at its end"
        );
    }
}

#[test]
fn nested_title_bands_have_the_recorded_literal_geometry() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart TB\nsubgraph outer[Outer]\nsubgraph inner[Inner]\nA\nend\nend\n"),
    );
    assert!(prepared.glides.is_empty());
    assert_rect(
        bounds(&prepared.after, "outer"),
        [124.0, 32.0, 112.0, 124.0],
    );
    assert_rect(bounds(&prepared.after, "inner"), [132.0, 66.0, 96.0, 82.0]);
    assert_rect(bounds(&prepared.after, "A"), [140.0, 100.0, 80.0, 40.0]);
    let items = prepared.after.items();
    for (name, title, expected) in [
        ("outer", "Outer", [90.0_f32, 40.0]),
        ("inner", "Inner", [90.0, 74.0]),
    ] {
        assert_eq!(
            labelled(&items, title).handle,
            scene.handle(&id(name)).expect("named group")
        );
        let origin = title_origin(&prepared.after, title);
        assert_eq!(
            [origin.x, origin.y].map(f32::to_bits),
            expected.map(f32::to_bits)
        );
    }
    assert_eq!(scene.object_count(), 5);
}

#[test]
fn a_long_private_title_widens_its_group_without_a_text_child() {
    let title = "A title that is intentionally longer than the node";
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph(&format!("flowchart TB\nsubgraph wide[{title}]\nA\nend\n")),
    );
    assert!(prepared.glides.is_empty());
    assert_rect(bounds(&prepared.after, "wide"), [82.0, 32.0, 196.0, 82.0]);
    assert_rect(bounds(&prepared.after, "A"), [140.0, 66.0, 80.0, 40.0]);
    assert_eq!(
        labelled(&prepared.after.items(), title).handle,
        scene.handle(&id("wide")).expect("group")
    );
    let origin = title_origin(&prepared.after, title);
    assert_eq!(
        [origin.x, origin.y].map(f32::to_bits),
        [90.0_f32, 40.0].map(f32::to_bits)
    );
    assert_eq!(scene.object_count(), 4);
}

#[test]
fn quoted_one_word_groups_are_named_and_multiword_groups_are_distinct() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph(
            "flowchart TB\nsubgraph \"Same Title\"\nA\nend\nsubgraph \"Same Title\"\nB\nend\nsubgraph \"Same\"\nC\nend\n",
        ),
    );
    let items = prepared.after.items();
    let anonymous: Vec<_> = items
        .iter()
        .filter(|item| {
            item.primitives.iter().any(|primitive|
        matches!(primitive, Prim::Text { text, .. } if text.as_str() == "Same Title")
    )
        })
        .map(|item| item.handle)
        .collect();
    assert_eq!(anonymous.len(), 2);
    assert_ne!(anonymous[0], anonymous[1]);
    let named = scene.handle(&id("Same")).expect("one-word named group");
    assert_eq!(labelled(&items, "Same").handle, named);
    assert_eq!(
        items
            .iter()
            .find(|item| Some(item.handle) == scene.handle(&id("C")))
            .expect("C")
            .parent,
        Some(named)
    );
    assert_eq!(scene.object_count(), 8);
    let empty = scene_support::new_scene("reopening");
    rejected(
        &empty,
        &GlobalBudget::default(),
        &graph("flowchart TB\nsubgraph \"Same\"\nA\nend\nsubgraph \"Same\"\nB\nend\n"),
        ErrorCode::Unsupported,
        "put[1].mermaid.src",
    );
}

#[test]
fn chained_fanout_preserves_first_mention_handles_and_unicode_labels() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart LR\nA[α] & B[β] -->|join| C[γ] & D[δ] --> E[終]\n"),
    );
    let items = prepared.after.items();
    assert_eq!(scene.object_count(), 13);
    let frame = at_rest(&prepared.after);
    let mut prior = None;
    for (from, to, label) in [
        ("A", "C", Some("join")),
        ("A", "D", Some("join")),
        ("B", "C", Some("join")),
        ("B", "D", Some("join")),
        ("C", "E", None),
        ("D", "E", None),
    ] {
        let handle = edge_handle(&prepared.after, &frame, from, to);
        assert!(
            prior.is_none_or(|previous| handle > previous),
            "expanded source order"
        );
        prior = Some(handle);
        let rendered = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("prepared edge");
        assert!(
            rendered
                .primitives
                .iter()
                .filter_map(|prim| match prim {
                    Prim::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .eq(label)
        );
    }
    for ((name, label), slot) in [("A", "α"), ("B", "β"), ("C", "γ"), ("D", "δ"), ("E", "終")]
        .into_iter()
        .zip(2..7)
    {
        let handle = scene.handle(&id(name)).expect("source node");
        assert_eq!(handle.slot(), slot);
        assert_eq!(labelled(&items, label).handle, handle);
    }
}

#[test]
fn compilation_errors_remap_shifted_plain_ops_and_mermaid_targets() {
    let scene = scene();
    let budget = GlobalBudget::default();
    rejected(
        &scene,
        &budget,
        &typed(&json!({"id":"shifted","put":[
            {"diagram":{"id":"target"}},
            {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\n"}},
            {"line":{"id":"bad","from":{"ref":"missing"},"to":[0,0]}}
        ]})),
        ErrorCode::UnknownRef,
        "put[2].line.from.ref",
    );
    rejected(
        &scene,
        &budget,
        &typed(&json!({"id":"target_missing","put":[
            {"mermaid":{"in":"missing","src":"flowchart LR\nA --> B\n"}}
        ]})),
        ErrorCode::UnknownRef,
        "put[0].mermaid.in",
    );
    rejected(
        &scene,
        &budget,
        &typed(&json!({"id":"target_kind","put":[
            {"table":{"id":"target","box":[0,0,100,40],"cols":["C"]}},
            {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\n"}}
        ]})),
        ErrorCode::OutOfRange,
        "put[1].mermaid.in",
    );
    let edges = format!("flowchart LR\n{}", "A --> B\n".repeat(97));
    rejected(
        &scene,
        &budget,
        &graph(&edges),
        ErrorCode::OverBudget(Budget::DiagramEdges),
        "put[1].mermaid.src",
    );
}

#[test]
fn failed_compilation_reserves_no_step_or_handle_identity() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        &typed(&json!({"id":"keep","put":[
            {"diagram":{"id":"target"}}
        ]})),
    );
    let invalid = typed(&json!({"id":"retry","put":[
        {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\nstyle A fill:red\n"}}
    ]}));
    rejected(
        &scene,
        &budget,
        &invalid,
        ErrorCode::Unsupported,
        "put[0].mermaid.src",
    );
    let valid = typed(&json!({"id":"retry","put":[
        {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\n"}}
    ]}));
    let second = picture(&mut scene, &mut budget, &first.after, &valid);
    assert_eq!(second.step(), &valid);
    assert_eq!(scene.step(1), Some(&valid));
    for (name, slot) in [("A", 1), ("B", 2)] {
        let handle = scene.handle(&id(name)).expect("retry node");
        assert_eq!(handle.revision(), 2);
        assert_eq!(handle.slot(), slot);
    }
    assert_eq!(scene.object_count(), 5);
    assert_eq!(budget.usage().objects, 5);
}

#[test]
fn mermaid_targets_can_be_declared_later_in_the_original_step() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let step = typed(&json!({"id":"forward_target","put":[
        {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\n"}},
        {"diagram":{"id":"target"}}
    ]}));
    let prepared = picture(&mut scene, &mut GlobalBudget::default(), &before, &step);
    assert_eq!(prepared.step(), &step);
    assert_eq!(scene.object_count(), 5);
    assert_eq!(scene.handle(&id("target")).expect("target").slot(), 4);
    assert_rect(bounds(&prepared.after, "A"), [64.0, 32.0, 80.0, 40.0]);
    assert_rect(bounds(&prepared.after, "B"), [216.0, 32.0, 80.0, 40.0]);
}

fn full_graph() -> String {
    let mut source = String::from("flowchart TB\n");
    for group in 0..13 {
        writeln!(source, "subgraph g{group}").expect("source string");
        for node in 0..45 {
            writeln!(source, "n{}", group * 45 + node).expect("source string");
        }
        source.push_str("end\n");
    }
    source
}

#[test]
fn expanded_objects_reach_600_without_double_charging_the_source_op() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let step = graph(&full_graph());
    let Admission::New(candidate) = scene.admit(&step, &budget).expect("600 expanded objects")
    else {
        panic!("new graph");
    };
    let committed = scene.commit(candidate, &mut budget).expect("commit");
    assert_eq!(committed.handles.len(), 600);
    assert_eq!(scene.object_count(), 600);
    assert_eq!(budget.usage().objects, 600);
    assert_eq!(scene.step(0), Some(&step));
    assert!(matches!(
        scene.admit(&step, &budget),
        Ok(Admission::Duplicate)
    ));
    assert_eq!(budget.usage().objects, 600);
    rejected(
        &scene,
        &budget,
        &typed(&json!({"id":"overflow","put":[
            {"text":{"id":"extra","at":[0,0],"text":"x"}}
        ]})),
        ErrorCode::OverBudget(Budget::LiveObjects),
        "$",
    );
}

#[test]
fn generated_names_do_not_alias_plain_ids_or_other_mermaid_blocks() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let step = typed(&json!({"id":"names","put":[
        {"diagram":{"id":"target"}},
        {"shape":{"id":"--1.2","kind":"rect","box":[0,0,10,10]}},
        {"mermaid":{"in":"target","src":"flowchart LR\nA --> B\n"}},
        {"shape":{"id":"--1.7","kind":"rect","box":[0,0,10,10]}},
        {"mermaid":{"in":"target","src":"flowchart LR\nC --> D\n"}}
    ]}));
    let prepared = picture(&mut scene, &mut GlobalBudget::default(), &before, &step);
    assert_eq!(prepared.step(), &step);
    assert_eq!(scene.object_count(), 11);
    for (name, slot) in [
        ("--1.2", 1),
        ("A", 3),
        ("B", 4),
        ("--1.7", 6),
        ("C", 8),
        ("D", 9),
    ] {
        assert_eq!(
            scene.handle(&id(name)).expect("verbatim source id").slot(),
            slot
        );
    }
    let (_, recovered) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("collision-free replay");
    assert_eq!(recovered.items(), prepared.after.items());
}

#[test]
fn quoted_group_ids_keep_valid_wire_spelling_even_in_the_generated_namespace() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart TB\nsubgraph \"--1.1\"\nA\nend\n"),
    );
    let group = scene
        .handle(&id("--1.1"))
        .expect("verbatim quoted group id");
    assert_eq!(group.slot(), 2);
    assert_eq!(scene.object_count(), 4);
    let items = prepared.after.items();
    assert_eq!(labelled(&items, "--1.1").handle, group);
    assert_eq!(labelled(&items, "A").parent, Some(group));
}

fn nested_graph(groups: usize) -> String {
    let mut source = String::from("flowchart TB\n");
    for depth in 0..groups {
        writeln!(source, "subgraph g{depth}").expect("source string");
    }
    source.push_str("A\n");
    source.push_str(&"end\n".repeat(groups));
    source
}

#[test]
fn generated_reference_depth_16_is_admitted_without_lowering_the_limit() {
    for groups in [14, 16] {
        rejected(
            &scene(),
            &GlobalBudget::default(),
            &graph(&nested_graph(groups)),
            ErrorCode::TooDeep,
            "put[1].mermaid.src",
        );
    }
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let step = graph(&nested_graph(13));
    let Admission::New(candidate) = scene.admit(&step, &budget).expect("exact graph depth 16")
    else {
        panic!("new graph");
    };
    scene.commit(candidate, &mut budget).expect("commit");
    for node in (0..13)
        .map(|depth| format!("g{depth}"))
        .chain(["A".to_owned()])
    {
        assert!(scene.handle(&id(&node)).is_some(), "retained {node}");
    }
}

#[test]
fn expanded_put_slots_accept_856_and_refuse_857_before_handles() {
    for (animations, total) in [(256, 856), (257, 857)] {
        let mut value = json!({"id":"slots","put":[
            {"diagram":{"id":"target"}},
            {"mermaid":{"in":"target","src":full_graph()}}
        ]});
        value["put"]
            .as_array_mut()
            .expect("put array")
            .extend((0..animations).map(|target| {
                json!({"anim":{"target":format!("n{target}"),"track":"opacity","keys":[[0,1]]}})
            }));
        let step = typed(&value);
        let mut scene = scene();
        let mut budget = GlobalBudget::default();
        if total == 857 {
            rejected(
                &scene,
                &budget,
                &step,
                ErrorCode::TooLong,
                "put[1].mermaid.src",
            );
            continue;
        }
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("856 expanded slots")
        else {
            panic!("new step");
        };
        let committed = scene.commit(candidate, &mut budget).expect("commit");
        assert_eq!(committed.handles.len(), 600);
        assert_eq!(
            scene.handle(&id("n584")).expect("last source node").slot(),
            599
        );
        assert_eq!(budget.usage().objects, 600);
        assert_eq!(scene.step(0), Some(&step));
    }
}

#[test]
fn source_text_and_identifiers_keep_their_exact_boundaries() {
    let label = "🌱".repeat(400);
    let name = "n".repeat(40);
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph(&format!("flowchart TB\n{name}[\"{label}\"]\n")),
    );
    assert_eq!(scene.object_count(), 3);
    assert_eq!(
        labelled(&prepared.after.items(), &label).handle,
        scene.handle(&id(&name)).expect("40-character source id")
    );
    let empty = new_scene("text_limits");
    for source in [
        format!("flowchart TB\nA[\"{}\"]\n", "🌱".repeat(401)),
        format!("flowchart TB\n{}\n", "n".repeat(41)),
    ] {
        rejected(
            &empty,
            &GlobalBudget::default(),
            &graph(&source),
            ErrorCode::TooLong,
            "put[1].mermaid.src",
        );
    }
}

#[test]
fn bare_membership_mentions_do_not_redeclare_a_shaped_node() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("graph TD\nA(Christmas) -->|Get money| B[Go shopping]\nsubgraph 1test\nA\nend"),
    );
    assert_eq!(scene.object_count(), 6);
    let items = prepared.after.items();
    assert_eq!(
        labelled(&items, "Christmas").parent,
        scene.handle(&id("1test"))
    );
    assert_eq!(
        labelled(&items, "Christmas").handle,
        scene.handle(&id("A")).expect("source A")
    );
    let corner_radius = |node: &Item| {
        node.primitives
            .iter()
            .find_map(|prim| match prim {
                Prim::Rect {
                    radius,
                    border: Some(_),
                    ..
                } => Some(*radius),
                _ => None,
            })
            .expect("node outline")
    };
    assert!(
        corner_radius(labelled(&items, "Christmas"))
            > corner_radius(labelled(&items, "Go shopping"))
    );
}

#[test]
fn style_inside_an_unquoted_group_title_is_plain_text() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("graph TD;A-->B;subgraph old style that is broken;c-->d;end;"),
    );
    assert_eq!(scene.object_count(), 9);
    assert_eq!(
        labelled(&prepared.after.items(), "old style that is broken")
            .handle
            .slot(),
        2
    );
}

#[test]
fn a_bare_reference_in_a_later_group_keeps_first_closing_ownership() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart TB\nsubgraph first\nA\nend\nsubgraph second\nA\nend\n"),
    );
    assert_eq!(scene.object_count(), 5);
    let items = prepared.after.items();
    assert_eq!(labelled(&items, "A").parent, scene.handle(&id("first")));
    assert_eq!(
        labelled(&items, "second").handle,
        scene.handle(&id("second")).expect("second group")
    );
}

#[test]
fn generated_names_do_not_capture_a_dangling_plain_parent() {
    rejected(
        &scene(),
        &GlobalBudget::default(),
        &typed(&json!({"id":"graph","put":[
            {"diagram":{"id":"target"}},
            {"mermaid":{"in":"target","src":"flowchart TB\nA\n"}},
            {"text":{"id":"caption","in":"--1.0","at":[0,0],"text":"caption"}}
        ]})),
        ErrorCode::UnknownRef,
        "put[2].text.in",
    );
}

#[test]
fn a_titled_group_recomputes_its_band_after_a_later_node() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        &graph("flowchart TB\nsubgraph g[Group]\nA\nend\n"),
    );
    let next = picture(
        &mut scene,
        &mut budget,
        &first.after,
        &typed(&json!({"id":"grow","put":[
            {"node":{"id":"B","in":"g","label":"B"}},
            {"edge":{"id":"next","in":"g","from":"A","to":"B"}}
        ]})),
    );
    assert_rect(bounds(&next.after, "A"), [140.0, 66.0, 80.0, 40.0]);
    assert_rect(bounds(&next.after, "B"), [16.0, 178.0, 80.0, 40.0]);
    assert_rect(bounds(&next.after, "g"), [8.0, 32.0, 220.0, 194.0]);
    assert_eq!(title_origin(&next.after, "Group"), Vec2::new(28.0, 40.0));
    let items = next.after.items();
    let group = labelled(&items, "Group");
    let border = group
        .primitives
        .iter()
        .find_map(|prim| match prim {
            Prim::Rect {
                rect,
                border: Some(_),
                ..
            } => Some(*rect),
            _ => None,
        })
        .expect("visible group border");
    let transform = world_transform(&items, group);
    let origin = transform.point(border.origin);
    let world = Rect {
        origin,
        size: transform.point(border.max()) - origin,
    };
    assert_rect(world, [8.0, 32.0, 220.0, 194.0]);
}

#[test]
fn later_nodes_take_the_next_rank_in_reversed_groups() {
    for direction in ["RL", "BT"] {
        let mut scene = scene();
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let first = picture(
            &mut scene,
            &mut budget,
            &before,
            &graph(&format!(
                "flowchart {direction}\nsubgraph g[Group]\nA --> B\nend\n"
            )),
        );
        let next = picture(
            &mut scene,
            &mut budget,
            &first.after,
            &typed(&json!({"id":"grow","put":[
                {"node":{"id":"C","in":"g","label":"C"}},
                {"edge":{"id":"next","in":"g","from":"B","to":"C"}}
            ]})),
        );
        for name in ["A", "B"] {
            assert_eq!(bounds(&next.after, name), bounds(&first.after, name));
        }
        let a = bounds(&next.after, "A");
        let b = bounds(&next.after, "B");
        let c = bounds(&next.after, "C");
        if direction == "RL" {
            assert!(c.max().x < b.origin.x);
            assert_eq!(
                (b.origin.x - c.max().x).to_bits(),
                (a.origin.x - b.max().x).to_bits()
            );
        } else {
            assert!(c.max().y < b.origin.y);
            assert_eq!(
                (b.origin.y - c.max().y).to_bits(),
                (a.origin.y - b.max().y).to_bits()
            );
        }
    }
}

#[test]
fn a_title_only_subgraph_has_a_single_literal_band() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph("flowchart TB\nsubgraph g[T]\nend\n"),
    );
    assert_eq!(scene.object_count(), 3);
    assert_rect(bounds(&prepared.after, "g"), [168.0, 32.0, 24.0, 42.0]);
    assert_eq!(title_origin(&prepared.after, "T"), Vec2::new(90.0, 40.0));
}

#[test]
fn retained_prefix_residency_keeps_private_title_text() {
    let [short, long] = ["X", "Title"].map(|title| {
        let (retained, _, [allocated, freed]) = allocation::usage(|| {
            let mut scene = scene();
            let before = LayoutState::new(&scene);
            let mut budget = GlobalBudget::default();
            let retained = picture(
                &mut scene,
                &mut budget,
                &before,
                &graph(&format!("flowchart TB\nsubgraph g[{title}]\nA\nend\n")),
            )
            .after;
            assert_eq!(scene.object_count(), 4);
            let group = retained.handle(&id("g")).expect("named group");
            let next = picture(
                &mut scene,
                &mut budget,
                &retained,
                &typed(&json!({"id":"later","put":[
                    {"node":{"id":"B","in":"g","label":"B"}}
                ]})),
            );
            assert_eq!(labelled(&next.after.items(), title).handle, group);
            retained
        });
        (allocated - freed, retained.resident_bytes())
    });
    assert!(
        long.0 + short.1 <= short.0 + long.1,
        "charge covers retained title growth: {short:?} -> {long:?}"
    );
}

#[test]
fn prepared_residency_keeps_the_private_compiled_node_text() {
    let [short, long] = ["X", "Label"].map(|label| {
        let mut scene = scene();
        let before = LayoutState::new(&scene);
        let mut budget = GlobalBudget::default();
        let step = graph(&format!("flowchart TB\nA[{label}]\n"));
        let Admission::New(admission) = scene.admit(&step, &budget).expect("graph admission")
        else {
            panic!("new graph")
        };
        let ticket = scene
            .commit(admission, &mut budget)
            .expect("commit")
            .preparation;
        let theme = Arc::new(Theme::default());
        let (prepared, _, [allocated, freed]) = allocation::usage(|| {
            let input = PrepareInput::new(
                ticket,
                before.clone(),
                SurfaceGeometry::default(),
                Arc::clone(&theme),
            )
            .expect("input");
            prepare(input, &mut Measure, &AtomicBool::new(false)).expect("prepare")
        });
        assert_eq!(scene.object_count(), 3);
        let handle = scene.handle(&id("A")).expect("named node");
        let frame = at_rest(&prepared.after);
        assert!(frame.paint_items().any(|(item, _, prims)| {
            item.handle == handle
                && prims
                    .iter()
                    .any(|prim| matches!(prim, Prim::Text { text, .. } if text.as_str() == label))
        }));
        drop(frame);
        let charged = prepared.resident_bytes;
        assert!(allocated - freed <= charged);
        let ((), _, [_, held]) = allocation::usage(|| drop(prepared));
        (held, charged)
    });
    assert!(
        long.0 + short.1 <= short.0 + long.1,
        "charge covers retained label growth: {short:?} -> {long:?}"
    );
}

#[test]
fn the_connected_mermaid_measurement_really_prepares_600_objects() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        &graph(&connected_full_graph()),
    );
    assert_eq!(scene.object_count(), 600);
    assert_eq!(prepared.after.revealed_at_rest().len(), 600);
    assert_eq!(scene.handle(&id("n298")).expect("last node").slot(), 307);
    let frame = at_rest(&prepared.after);
    let mut start = 0;
    for size in [48, 48, 48, 48, 48, 48, 11] {
        for n in start..start + size - 1 {
            edge_handle(
                &prepared.after,
                &frame,
                &format!("n{n}"),
                &format!("n{}", n + 1),
            );
        }
        start += size;
    }
}
