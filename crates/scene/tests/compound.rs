//! Compound graph admission, placement and legal-cap preparation measurements.
use kumochi_scene::{
    Admission, Budget, ErrorCode, Frame, GlobalBudget, Record, SceneState, SceneView, StopCut,
    display::Prim,
    geom::{Rect, Vec2},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare, recover, recover_all},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[path = "support/compound.rs"]
mod fixtures;
#[path = "support/scene.rs"]
mod scene_support;

use scene_support::{
    Measure, id, new_scene as named_scene, prepared as picture, prepared_on as picture_on,
};

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("safe bounds")
        .expect("object")
}

fn assert_rect(actual: Rect, origin: [f32; 2], size: [f32; 2]) {
    assert_eq!(
        [
            actual.origin.x,
            actual.origin.y,
            actual.size.x,
            actual.size.y
        ]
        .map(f32::to_bits),
        [origin[0], origin[1], size[0], size[1]].map(f32::to_bits),
        "the ranked rectangle has its derived position and size"
    );
}

fn rejected(scene: &SceneState, budget: &GlobalBudget, value: Value, code: ErrorCode) {
    let before = (
        scene.revision(),
        scene.usage(),
        scene.recovery_log().len(),
        budget.usage(),
        budget.generation(),
    );
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let bytes = serde_json::to_vec(&envelope).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    assert_eq!(
        scene.admit(&step, budget).expect_err("rejected step").code,
        code,
        "the intended admission limit refuses the step"
    );
    assert_eq!(
        (
            scene.revision(),
            scene.usage(),
            scene.recovery_log().len(),
            budget.usage(),
            budget.generation(),
        ),
        before,
        "failed admission leaves the scene and global budget unchanged"
    );
}

fn scene() -> SceneState {
    named_scene("compound")
}

fn groups() -> Vec<Value> {
    vec![
        json!({"diagram":{"id":"outer"}}),
        json!({"diagram":{"id":"left","in":"outer"}}),
        json!({"diagram":{"id":"deep","in":"left"}}),
        json!({"diagram":{"id":"right","in":"outer"}}),
        json!({"node":{"id":"a","in":"deep","label":"A"}}),
        json!({"node":{"id":"b","in":"right","label":"B"}}),
        json!({"node":{"id":"direct","in":"left","label":"Direct"}}),
        json!({"node":{"id":"c","in":"deep","label":"C"}}),
    ]
}

#[test]
fn edges_belong_to_the_exact_least_common_diagram_of_their_nodes() {
    for (from, to, parent) in [
        ("a", "b", "outer"),
        ("a", "direct", "left"),
        ("a", "c", "deep"),
        ("a", "a", "deep"),
    ] {
        let mut scene = scene();
        let before = LayoutState::new(&scene);
        let mut put = groups();
        put.push(json!({"edge":{"id":"edge","in":parent,"from":from,"to":to}}));
        let prepared = picture(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"edges","put":put}),
        );
        let handle = prepared.after.handle(&id("edge")).expect("prepared edge");
        let items = prepared.after.items();
        let edge = items
            .iter()
            .find(|item| item.handle == handle)
            .expect("edge item");
        assert!(
            edge.primitives
                .iter()
                .any(|prim| matches!(prim, Prim::Line { points, .. } if points.len() >= 2)),
            "the accepted least-common-group edge {from} → {to} is drawn"
        );
    }
}

#[test]
fn wrong_group_and_non_node_endpoints_fail_without_mutating_the_scene() {
    for (from, to, parent, code, field) in [
        ("a", "b", "left", ErrorCode::OutOfRange, "from"),
        ("a", "c", "outer", ErrorCode::OutOfRange, "from"),
        ("a", "b", "right", ErrorCode::OutOfRange, "from"),
        ("left", "b", "outer", ErrorCode::OutOfRange, "from"),
        ("a", "right", "outer", ErrorCode::OutOfRange, "to"),
        ("a", "missing", "outer", ErrorCode::UnknownRef, "to"),
        ("missing", "b", "outer", ErrorCode::UnknownRef, "from"),
    ] {
        let scene = scene();
        let budget = GlobalBudget::default();
        let mut put = groups();
        put.push(json!({"edge":{"id":"edge","in":parent,"from":from,"to":to}}));
        let bytes =
            serde_json::to_vec(&json!({"step":{"id":"edges","put":put}})).expect("step JSON");
        let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
            panic!("step fixture");
        };
        let error = scene.admit(&step, &budget).expect_err("invalid edge");
        assert_eq!(error.code, code);
        assert_eq!(error.path.as_str(), format!("put[8].edge.{field}"));
        assert_eq!(scene.object_count(), 0);
        assert_eq!(scene.revision(), 0);
        assert_eq!(scene.recovery_log().len(), 0);
        assert_eq!(budget.usage().objects, 0);
    }
}

#[test]
fn a_table_is_not_an_implicit_graph_cluster() {
    let scene = scene();
    let budget = GlobalBudget::default();
    let bytes = serde_json::to_vec(&json!({"step":{"id":"mixed","put":[
        {"diagram":{"id":"outer"}},
        {"table":{"id":"table","in":"outer","cols":["V"]}},
        {"diagram":{"id":"nested","in":"table"}},
        {"node":{"id":"a","in":"nested","label":"A"}},
        {"node":{"id":"b","in":"outer","label":"B"}},
        {"edge":{"id":"edge","in":"outer","from":"a","to":"b"}}
    ]}}))
    .expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let error = scene
        .admit(&step, &budget)
        .expect_err("non-diagram barrier");
    assert_eq!(error.code, ErrorCode::OutOfRange);
    assert_eq!(error.path.as_str(), "put[5].edge.from");
    assert_eq!(scene.object_count(), 0);
    assert_eq!(budget.usage().objects, 0);
}

#[test]
fn nested_groups_inherit_direction_and_an_explicit_child_direction_wins() {
    for (outer, inner, offset) in [
        ("right", None, Vec2::new(152.0, 0.0)),
        ("down", None, Vec2::new(0.0, 112.0)),
        ("down", Some("right"), Vec2::new(152.0, 0.0)),
        ("right", Some("down"), Vec2::new(0.0, 112.0)),
    ] {
        let mut scene = scene();
        let before = LayoutState::new(&scene);
        let prepared = picture(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"nested","put":[
                {"diagram":{"id":"outer","dir":outer}},
                {"diagram":{"id":"inner","in":"outer","dir":inner}},
                {"diagram":{"id":"deep","in":"inner"}},
                {"node":{"id":"a","in":"deep","label":"A"}},
                {"node":{"id":"b","in":"deep","label":"B"}},
                {"edge":{"id":"edge","in":"deep","from":"a","to":"b"}}
            ]}),
        );
        let a = bounds(&prepared.after, "a");
        let b = bounds(&prepared.after, "b");
        assert_eq!(
            [a.size.x, a.size.y, b.size.x, b.size.y].map(f32::to_bits),
            [80.0_f32, 40.0, 80.0, 40.0].map(f32::to_bits)
        );
        let delta = b.origin - a.origin;
        assert_eq!(
            [delta.x, delta.y].map(f32::to_bits),
            [offset.x, offset.y].map(f32::to_bits)
        );
    }
}

#[test]
fn clusters_measure_table_only_and_boxed_diagram_only_contents_before_ranking() {
    for table in [true, false] {
        let mut scene = scene();
        let before = LayoutState::new(&scene);
        let child = if table {
            json!({"table":{"id":"content","in":"left","box":[0,0,180,120],"cols":["ABCDEFGHIJKLMNOPQRST"]}})
        } else {
            json!({"diagram":{"id":"content","in":"left","box":[0,0,180,120]}})
        };
        let prepared = picture(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            json!({"id":"contents","put":[
                {"diagram":{"id":"outer","box":[0,0,360,300]}},
                {"diagram":{"id":"left","in":"outer"}},
                child,
                {"diagram":{"id":"right","in":"outer"}},
                {"node":{"id":"b","in":"right","label":"B"}}
            ]}),
        );
        let left = bounds(&prepared.after, "left");
        let right = bounds(&prepared.after, "right");
        if table {
            assert!((left.origin.x - 19.0).abs() < 0.01);
            assert!((left.origin.y - 36.225).abs() < 0.01);
            assert!((left.size.x - 180.0).abs() < 0.01);
            assert!((left.size.y - 31.55).abs() < 0.01);
            assert_rect(right, [261.0, 32.0], [80.0, 40.0]);
        } else {
            assert_rect(left, [19.0, 32.0], [180.0, 120.0]);
            assert_rect(right, [261.0, 72.0], [80.0, 40.0]);
        }
        assert_eq!(bounds(&prepared.after, "content"), left);
    }
}

#[test]
fn a_boxed_child_keeps_its_box_outside_its_parents_ranking() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"boxed","put":[
            {"diagram":{"id":"outer"}},
            {"diagram":{"id":"boxed","in":"outer","box":[10,200,100,60]}},
            {"node":{"id":"a","in":"outer","label":"A"}},
            {"node":{"id":"b","in":"outer","label":"B"}}
        ]}),
    );
    assert_rect(
        bounds(&prepared.after, "boxed"),
        [10.0, 200.0],
        [100.0, 60.0],
    );
    assert_rect(bounds(&prepared.after, "a"), [85.0, 16.0], [80.0, 40.0]);
    assert_rect(bounds(&prepared.after, "b"), [195.0, 16.0], [80.0, 40.0]);
}

#[test]
fn a_nested_overlay_group_inherits_the_surface_default_direction() {
    let bytes = serde_json::to_vec(&json!({"begin":{
        "v":1,"scene":"overlay","surface":"overlay","alt":""
    }}))
    .expect("header JSON");
    let Record::Begin(begin) = parse_record(&bytes).expect("begin") else {
        panic!("begin fixture");
    };
    let mut scene = SceneState::begin(begin).expect("overlay scene");
    let before = LayoutState::new(&scene);
    let prepared = picture_on(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"overlay","put":[
            {"diagram":{"id":"outer"}},
            {"diagram":{"id":"inner","in":"outer"}},
            {"diagram":{"id":"deep","in":"inner"}},
            {"node":{"id":"a","in":"deep","label":"A"}},
            {"node":{"id":"b","in":"deep","label":"B"}},
            {"edge":{"id":"ab","in":"deep","from":"a","to":"b"}}
        ]}),
        SurfaceGeometry::Overlay {
            default_canvas: [800.0, 600.0],
        },
    );
    let a = bounds(&prepared.after, "a");
    let b = bounds(&prepared.after, "b");
    assert_eq!((b.origin.x - a.origin.x).to_bits(), 152.0_f32.to_bits());
    assert_eq!(b.origin.y.to_bits(), a.origin.y.to_bits());
    assert_eq!(
        [a.size.x, a.size.y, b.size.x, b.size.y].map(f32::to_bits),
        [80.0_f32, 40.0, 80.0, 40.0].map(f32::to_bits)
    );
}

#[test]
fn a_cross_group_edge_ranks_clusters_and_routes_between_world_space_endpoints() {
    let mut scene = scene();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"cross","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"left","in":"outer"}},
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"a","in":"left","label":"A"}},
            {"node":{"id":"b","in":"right","label":"B"}},
            {"edge":{"id":"edge","in":"outer","from":"a","to":"b"}}
        ]}),
    );
    let a = bounds(&prepared.after, "a");
    let b = bounds(&prepared.after, "b");
    assert_eq!(b.origin - a.origin, Vec2::new(184.0, 0.0));
    assert_eq!(
        bounds(&prepared.after, "right").origin.x.to_bits(),
        (bounds(&prepared.after, "left").max().x + 104.0).to_bits()
    );
    let view = SceneView::new(&prepared.after, 1);
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("rest frame");
    let handle = prepared.after.handle(&id("edge")).expect("edge handle");
    let edge = frame
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .expect("edge item");
    let points = edge
        .primitives
        .iter()
        .find_map(|prim| match prim {
            Prim::Line {
                points,
                closed: false,
                ..
            } => Some(points),
            _ => None,
        })
        .expect("edge path");
    assert_eq!(
        edge.transform.point(points[0]),
        Vec2::new(a.max().x, a.center().y)
    );
    assert_eq!(
        edge.transform.point(*points.last().expect("path endpoint")),
        Vec2::new(b.origin.x, b.center().y)
    );
}

#[test]
fn a_diagram_can_rank_48_direct_nodes_alongside_child_clusters() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut put = vec![json!({"diagram":{"id":"outer"}})];
    put.extend((0..48).map(|index| {
        json!({"node":{
            "id":format!("n{index}"),"in":"outer","label":"N"
        }})
    }));
    for group in ["left", "right"] {
        put.push(json!({"diagram":{"id":group,"in":"outer"}}));
        put.push(json!({"node":{"id":format!("{group}node"),"in":group,"label":"N"}}));
    }
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"node-caps","put":put}),
    );
    assert_eq!(scene.usage().objects, 53);
    assert_eq!(prepared.after.items().len(), 53);
    let members: Vec<_> = (0..48)
        .map(|index| bounds(&prepared.after, &format!("n{index}")))
        .chain(["left", "right"].map(|name| bounds(&prepared.after, name)))
        .collect();
    for (index, member) in members.iter().enumerate() {
        for other in &members[index + 1..] {
            assert!(
                !member.intersects(*other),
                "ranked members must not overlap"
            );
        }
    }
    rejected(
        &scene,
        &budget,
        json!({"id":"node49","put":[{"node":{"id":"extra","in":"outer","label":"N"}}]}),
        ErrorCode::OverBudget(Budget::DiagramNodes),
    );
}

fn assert_cross_group_routes(layout: &LayoutState, step: &Value) {
    let view = SceneView::new(layout, 1);
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("rest frame");
    let routes = step["put"]
        .as_array()
        .expect("authored operations")
        .iter()
        .filter_map(|op| op.get("edge"))
        .filter(|edge| edge["in"] == "g0");
    let mut checked = 0;
    for edge in routes {
        let name = edge["id"].as_str().expect("edge id");
        let a = bounds(layout, edge["from"].as_str().expect("from id"));
        let b = bounds(layout, edge["to"].as_str().expect("to id"));
        let handle = layout.handle(&id(name)).expect("edge handle");
        let item = frame
            .items()
            .iter()
            .find(|item| item.handle == handle)
            .expect("painted edge");
        let from = a.edge_toward(b.center());
        let to = b.edge_toward(a.center());
        let at = |point, expected: Vec2| {
            let delta = item.transform.point(point) - expected;
            delta.x.abs() < 0.01 && delta.y.abs() < 0.01
        };
        assert!(
            item.primitives.iter().any(|prim| {
                let Prim::Line {
                    points,
                    closed: false,
                    ..
                } = prim
                else {
                    return false;
                };
                points
                    .first()
                    .zip(points.last())
                    .is_some_and(|(a, b)| at(*a, from) && at(*b, to))
            }),
            "{name} connects the two node outlines"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 96,
        "the fixture contains every cross-cluster route"
    );
}

#[test]
fn compound_cap_fixtures_share_the_global_object_budget() {
    for fixture in [fixtures::Fixture::Local, fixtures::Fixture::CrossGroup] {
        let mut budget = GlobalBudget::default();
        let mut retained = Vec::new();
        for (index, count) in [600, 600, 150, 150].into_iter().enumerate() {
            let mut scene = named_scene(&format!("caps{index}"));
            let before = LayoutState::new(&scene);
            let step = fixtures::capped_step(count, fixture);
            let prepared = picture(&mut scene, &mut budget, &before, step.clone());
            assert_eq!(scene.usage().objects, count);
            assert_eq!(prepared.after.items().len(), count);
            if count == 600 && matches!(fixture, fixtures::Fixture::CrossGroup) {
                for name in ["0", "30", "40", "50"] {
                    let node = bounds(&prepared.after, name);
                    assert!(
                        (node.size.x - 64.0).abs() < 0.01,
                        "{name} fits at the 0.8 floor"
                    );
                    assert!(
                        (node.size.y - 32.0).abs() < 0.01,
                        "{name} fits at the 0.8 floor"
                    );
                }
                let clusters = ["g1", "ge", "gf", "g10"].map(|name| bounds(&prepared.after, name));
                for pair in clusters.windows(2) {
                    assert!((pair[1].origin.y - pair[0].max().y - 104.0 * 0.8).abs() < 0.01);
                    assert!((pair[1].center().x - pair[0].center().x).abs() < 0.01);
                }
                assert_cross_group_routes(&prepared.after, &step);
            }
            retained.push((scene, prepared));
        }
        assert_eq!(budget.usage().scenes, 4);
        assert_eq!(budget.usage().objects, 1500);
        assert_eq!(retained.len(), 4);
        rejected(
            &retained[2].0,
            &budget,
            json!({"id":"overflow","put":[{"line":{"id":"extra","from":[0,0],"to":[1,1]}}]}),
            ErrorCode::OverBudget(Budget::LiveObjects),
        );
        if matches!(fixture, fixtures::Fixture::CrossGroup) {
            let chain = growing_chain(fixture);
            let sources: Vec<_> = std::iter::once(&chain)
                .chain(retained.iter().skip(1).map(|(scene, _)| scene))
                .map(|scene| (scene.header(), scene.recovery_log()))
                .collect();
            let (results, recovered_budget) = recover_all(
                &sources,
                SurfaceGeometry::default(),
                &Arc::new(Theme::default()),
                &mut Measure,
                &AtomicBool::new(false),
                None,
            );
            let recovered: Vec<_> = results
                .into_iter()
                .map(|result| {
                    let (scene, layout) = result.expect("recovered capped view");
                    (scene.revision(), layout.items().len())
                })
                .collect();
            assert_eq!(recovered, [(64, 600), (1, 600), (1, 150), (1, 150)]);
            assert_eq!(recovered_budget.usage().objects, 1500);
        }
    }
}

#[test]
fn the_97th_outer_owned_edge_is_rejected_without_charging_the_budget() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let mut put = vec![
        json!({"diagram":{"id":"outer"}}),
        json!({"diagram":{"id":"left","in":"outer"}}),
        json!({"diagram":{"id":"right","in":"outer"}}),
        json!({"node":{"id":"a","in":"left","label":"A"}}),
        json!({"node":{"id":"b","in":"right","label":"B"}}),
    ];
    put.extend((0..96).map(|index| {
        json!({"edge":{
            "id":format!("e{index}"),"in":"outer","from":"a","to":"b"
        }})
    }));
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"edges","put":put}),
    );
    assert_rect(bounds(&prepared.after, "left"), [140.0, 32.0], [80.0, 40.0]);
    assert_rect(
        bounds(&prepared.after, "right"),
        [140.0, 176.0],
        [80.0, 40.0],
    );
    rejected(
        &scene,
        &budget,
        json!({"id":"edge97","put":[{"edge":{
            "id":"extra","in":"outer","from":"a","to":"b"
        }}]}),
        ErrorCode::OverBudget(Budget::DiagramEdges),
    );
}

#[test]
fn a_new_cluster_takes_a_free_slot_without_moving_earlier_nodes() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"left","in":"outer"}},
            {"node":{"id":"a","in":"left","label":"A"}},
            {"node":{"id":"b","in":"left","label":"B"}},
            {"edge":{"id":"ab","in":"left","from":"a","to":"b"}}
        ]}),
    );
    let a = bounds(&first.after, "a");
    let b = bounds(&first.after, "b");
    let second = picture(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"second","put":[
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"c","in":"right","label":"C"}}
        ]}),
    );
    assert_eq!(bounds(&second.after, "a"), a);
    assert_eq!(bounds(&second.after, "b"), b);
    assert_eq!(bounds(&second.after, "left"), bounds(&first.after, "left"));
    assert_eq!(
        bounds(&second.after, "right").origin.y.to_bits(),
        (bounds(&second.after, "left").max().y + 62.0).to_bits()
    );
}

#[test]
fn removing_a_cluster_node_releases_its_bound_before_a_sibling_is_added() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"outer","box":[0,0,264,1000]}},
            {"diagram":{"id":"left","in":"outer","dir":"right"}},
            {"node":{"id":"a","in":"left","label":"A"}},
            {"node":{"id":"b","in":"left","label":"B"}},
            {"edge":{"id":"ab","in":"left","from":"a","to":"b"}}
        ]}),
    );
    assert_rect(bounds(&first.after, "left"), [16.0, 32.0], [232.0, 40.0]);
    let removed = picture(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"remove","remove":["ab","b"]}),
    );
    assert_rect(bounds(&removed.after, "left"), [16.0, 32.0], [80.0, 40.0]);
    let added = picture(
        &mut scene,
        &mut budget,
        &removed.after,
        json!({"id":"sibling","put":[
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"c","in":"right","label":"C"}}
        ]}),
    );
    assert_rect(bounds(&added.after, "a"), [16.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&added.after, "left"), [16.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&added.after, "right"), [158.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&added.after, "c"), [158.0, 32.0], [80.0, 40.0]);
}

#[test]
fn a_new_diagram_prepares_when_the_step_removes_an_edge_that_ranked_clusters() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"outer","box":[0,0,1000,1000]}},
            {"diagram":{"id":"left","in":"outer"}},
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"a","in":"left","label":"A"}},
            {"node":{"id":"b","in":"right","label":"B"}},
            {"edge":{"id":"ab","in":"outer","from":"a","to":"b"}}
        ]}),
    );
    assert_rect(bounds(&first.after, "left"), [460.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&first.after, "right"), [460.0, 176.0], [80.0, 40.0]);
    let second = picture(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"second","remove":["ab"],"put":[
            {"diagram":{"id":"new","in":"outer"}},
            {"node":{"id":"c","in":"new","label":"C"}}
        ]}),
    );
    assert!(
        second
            .after
            .bounds(&id("ab"))
            .expect("safe bounds")
            .is_none()
    );
    assert_rect(bounds(&second.after, "a"), [460.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&second.after, "b"), [460.0, 176.0], [80.0, 40.0]);
    assert_rect(bounds(&second.after, "new"), [32.0, 32.0], [80.0, 40.0]);
    assert_rect(bounds(&second.after, "c"), [32.0, 32.0], [80.0, 40.0]);
}

#[test]
fn changing_a_clusters_colour_keeps_its_measured_size_for_new_siblings() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"first","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"cluster","in":"outer"}},
            {"node":{"id":"a","in":"cluster","label":"A"}},
            {"node":{"id":"b","in":"cluster","label":"B"}},
            {"edge":{"id":"ab","in":"cluster","from":"a","to":"b"}}
        ]}),
    );
    let cluster = bounds(&first.after, "cluster");
    let second = picture(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"second","set":[{"target":"cluster","color":"#abcdef"}],"put":[
            {"node":{"id":"sibling","in":"outer","label":"Sibling"}}
        ]}),
    );
    assert_eq!(bounds(&second.after, "cluster"), cluster);
    assert_eq!(
        bounds(&second.after, "sibling").origin.y.to_bits(),
        (cluster.max().y + 46.0).to_bits()
    );
}

#[test]
fn dropping_a_cluster_ancestor_preserves_world_bounds_and_recovery() {
    let mut scene = scene();
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let prepared = picture(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"cluster","put":[
            {"diagram":{"id":"outer"}},
            {"diagram":{"id":"inner","in":"outer"}},
            {"node":{"id":"a","in":"inner","label":"A"}},
            {"node":{"id":"b","in":"inner","label":"B"}},
            {"edge":{"id":"edge","in":"inner","from":"a","to":"b"}},
            {"anim":{"target":"outer","track":"x","keys":[[0,50]]}}
        ]}),
    );
    let survivors = ["inner", "a", "b", "edge"];
    let expected = survivors.map(|name| bounds(&prepared.after, name));
    let revealed = survivors.map(|name| prepared.after.handle(&id(name)).expect("survivor"));
    scene
        .stop(
            &StopCut {
                activated: 1,
                revealed: Bounded::new(revealed.to_vec()).expect("bounded cut"),
            },
            &mut budget,
        )
        .expect("ordered cut");
    let detached = prepared.after.cut(&scene).expect("detached cluster");
    let (_, recovered) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("recovered cluster");
    for layout in [&detached, &recovered] {
        assert_eq!(layout.items().len(), 4);
        assert_eq!(layout.handle(&id("outer")), None);
        for ((name, expected), handle) in survivors.into_iter().zip(expected).zip(revealed) {
            assert_eq!(bounds(layout, name), expected);
            assert_eq!(layout.handle(&id(name)), Some(handle));
        }
    }
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn prepare_four_views_at_caps() {
    use std::{hint::black_box, time::Instant};

    for fixture in [fixtures::Fixture::Local, fixtures::Fixture::CrossGroup] {
        let mut samples = Vec::with_capacity(64);
        for iteration in 0..69 {
            let mut budget = GlobalBudget::default();
            let mut inputs = Vec::with_capacity(4);
            let mut scenes = Vec::with_capacity(4);
            for (index, count) in [600, 600, 150, 150].into_iter().enumerate() {
                let mut scene = named_scene(&format!("caps{index}"));
                let before = LayoutState::new(&scene);
                let bytes =
                    serde_json::to_vec(&json!({"step":fixtures::capped_step(count, fixture)}))
                        .expect("step JSON");
                let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
                    panic!("step fixture");
                };
                let Admission::New(candidate) = scene.admit(&step, &budget).expect("cap admission")
                else {
                    panic!("new step");
                };
                let committed = scene
                    .commit(candidate, &mut budget)
                    .expect("fresh admission");
                inputs.push(
                    PrepareInput::new(
                        committed.preparation,
                        before,
                        SurfaceGeometry::default(),
                        Arc::new(Theme::default()),
                    )
                    .expect("initial picture"),
                );
                scenes.push(scene);
            }
            assert_eq!(budget.usage().objects, 1500);
            let mut outputs = Vec::with_capacity(4);
            let cancel = AtomicBool::new(false);
            let start = Instant::now();
            for input in inputs {
                outputs
                    .push(prepare(black_box(input), &mut Measure, &cancel).expect("cap picture"));
            }
            let elapsed = start.elapsed().as_secs_f64() * 1e6;
            assert_eq!(
                outputs
                    .iter()
                    .map(|step| step.after.items().len())
                    .sum::<usize>(),
                1500
            );
            assert_eq!(scenes.len(), 4);
            if iteration >= 5 {
                samples.push(elapsed);
            }
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        println!(
            "workload={} samples=64 p50_us={} p95_us={} p99_us={} max_us={}",
            if matches!(fixture, fixtures::Fixture::CrossGroup) {
                "compound-cross-at-caps"
            } else {
                "compound-control-at-caps"
            },
            sorted[31],
            sorted[60],
            sorted[63],
            sorted[63]
        );
        for (index, sample) in samples.into_iter().enumerate() {
            println!("sample {index} {sample:.3}");
        }
    }
}

fn committed_scene(name: &str, steps: impl IntoIterator<Item = Value>) -> SceneState {
    let mut scene = named_scene(name);
    let mut budget = GlobalBudget::default();
    for value in steps {
        let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
        assert!(
            bytes.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
            "fixture fits the record byte cap"
        );
        let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
            panic!("step fixture");
        };
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admitted step") else {
            panic!("new step");
        };
        scene
            .commit(candidate, &mut budget)
            .expect("fresh admission");
    }
    scene
}

/// The 14-diagram chain fixture, grown one batch per step through a 64-step history.
fn growing_chain(fixture: fixtures::Fixture) -> SceneState {
    let step = fixtures::capped_step(600, fixture);
    let put = step["put"].as_array().expect("fixture put list");
    committed_scene(
        "chain",
        (0..64).map(|batch| {
            let batch_put = &put[batch * put.len() / 64..(batch + 1) * put.len() / 64];
            json!({"id":format!("b{batch}"),"put":batch_put})
        }),
    )
}

/// 600 objects: a root, 17 clusters of 17 single-node clusters each, and four lines.
fn many_clusters() -> Value {
    let mut put = vec![json!({"diagram":{"id":"r","dir":"down"}})];
    for group in 0..17 {
        let owner = format!("m{group:x}");
        put.push(json!({"diagram":{"id":owner,"in":"r","dir":"right"}}));
        for member in 0..17 {
            let index = group * 17 + member;
            let cluster = format!("c{index:x}");
            put.push(json!({"diagram":{"id":cluster,"in":owner}}));
            put.push(json!({"node":{"id":format!("n{index:x}"),"in":cluster,"label":"N"}}));
        }
    }
    for index in 0..4 {
        put.push(json!({"line":{"id":format!("p{index}"),"from":[0,0],"to":[1,1]}}));
    }
    assert_eq!(put.len(), 600, "fixture reaches the object cap");
    json!({"id":"clusters","put":put})
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn recover_at_the_caps() {
    use std::{hint::black_box, time::Instant};

    let chain = growing_chain(fixtures::Fixture::Local);
    let cross = growing_chain(fixtures::Fixture::CrossGroup);
    let clusters = committed_scene("clusters", [many_clusters()]);
    let small_local = committed_scene(
        "small-local",
        [fixtures::capped_step(150, fixtures::Fixture::Local)],
    );
    let small_cross = committed_scene(
        "small-cross",
        [fixtures::capped_step(150, fixtures::Fixture::CrossGroup)],
    );
    assert_eq!((chain.revision(), cross.revision()), (64, 64));
    let theme = Arc::new(Theme::default());
    let cancel = AtomicBool::new(false);
    for (workload, scenes, objects) in [
        ("recover-growing-chain-control", vec![&chain], 600),
        ("recover-growing-chain-cross", vec![&cross], 600),
        ("recover-many-clusters", vec![&clusters], 600),
        (
            "recover-batch-chain-and-clusters",
            vec![&chain, &clusters],
            1200,
        ),
        (
            "recover-four-scenes-global-cap",
            vec![&chain, &clusters, &small_local, &small_cross],
            1500,
        ),
    ] {
        let sources: Vec<_> = scenes
            .iter()
            .map(|scene| (scene.header(), scene.recovery_log()))
            .collect();
        let mut samples = Vec::with_capacity(16);
        let mut retained = 0_usize;
        for iteration in 0..19 {
            let start = Instant::now();
            let (results, global) = recover_all(
                black_box(&sources),
                SurfaceGeometry::default(),
                &theme,
                &mut Measure,
                &cancel,
                None,
            );
            let elapsed = start.elapsed().as_secs_f64() * 1e3;
            assert_eq!(global.usage().objects, objects);
            retained = results
                .iter()
                .map(|result| {
                    let (_, layout) = result.as_ref().expect("recovered at the caps");
                    layout.resident_bytes()
                })
                .sum();
            if iteration >= 3 {
                samples.push(elapsed);
            }
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "workload={workload} samples=16 warmups=3 p50_ms={:.3} max_ms={:.3} resident_bytes={retained}; fixed text measurement, no shaping",
            samples[8], samples[15]
        );
    }
}
