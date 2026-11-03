//! Compound graph admission, placement and legal-cap preparation measurements.
use kumochi_scene::{
    Admission, ErrorCode, Frame, GlobalBudget, Record, SceneState, SceneView, StopCut, WireId,
    display::{MeasureError, Prim, TextMeasure, TextStyle},
    geom::{Rect, Vec2},
    layout::{LayoutState, PrepareInput, PreparedStep, SurfaceGeometry, prepare, recover},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[path = "support/compound.rs"]
mod fixtures;

struct Measure;

impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let width = f32::from(u16::try_from(text.chars().count()).expect("bounded text")) * 8.0;
        Ok([max_width.map_or(width, |limit| width.min(limit)), 10.0])
    }
}

fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("fixture id")
}

fn picture(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    before: &LayoutState,
    value: Value,
) -> PreparedStep {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let bytes = serde_json::to_vec(&envelope).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted step") else {
        panic!("new step");
    };
    let committed = scene.commit(candidate, budget).expect("fresh admission");
    let input = PrepareInput::new(
        committed.preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("consecutive preparation");
    let prepared = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("picture");
    prepared.validate(scene, before).expect("current picture");
    prepared
}

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("safe bounds")
        .expect("object")
}

fn scene() -> SceneState {
    named_scene("compound")
}

fn named_scene(name: &str) -> SceneState {
    let bytes = serde_json::to_vec(&json!({"begin":{
        "v":1,"scene":name,"surface":"inline","alt":""
    }}))
    .expect("header JSON");
    let Record::Begin(begin) = parse_record(&bytes).expect("begin record") else {
        panic!("begin fixture");
    };
    SceneState::begin(begin).expect("scene")
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
        let scene = scene();
        let budget = GlobalBudget::default();
        let mut put = groups();
        put.push(json!({"edge":{"id":"edge","in":parent,"from":from,"to":to}}));
        let bytes =
            serde_json::to_vec(&json!({"step":{"id":"edges","put":put}})).expect("step JSON");
        let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
            panic!("step fixture");
        };
        scene
            .admit(&step, &budget)
            .expect("least-common-group edge");
        assert_eq!(scene.object_count(), 0, "admission does not publish");
        assert_eq!(budget.usage().objects, 0);
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
                {"node":{"id":"a","in":"inner","label":"A"}},
                {"node":{"id":"b","in":"inner","label":"B"}},
                {"edge":{"id":"edge","in":"inner","from":"a","to":"b"}}
            ]}),
        );
        let a = bounds(&prepared.after, "a");
        let b = bounds(&prepared.after, "b");
        assert_eq!(a.size, Vec2::new(80.0, 40.0));
        assert_eq!(b.size, Vec2::new(80.0, 40.0));
        assert_eq!(b.origin - a.origin, offset);
    }
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
    assert!(bounds(&prepared.after, "left").max().x < bounds(&prepared.after, "right").origin.x);
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
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"node-caps","put":put}),
    );
    assert_eq!(scene.usage().objects, 53);
    assert_eq!(prepared.after.items().len(), 53);
}

#[test]
fn compound_cap_fixtures_share_the_global_object_budget() {
    for fixture in [fixtures::Fixture::Local, fixtures::Fixture::CrossGroup] {
        let mut budget = GlobalBudget::default();
        let mut retained = Vec::new();
        for (index, count) in [600, 600, 150, 150].into_iter().enumerate() {
            let mut scene = named_scene(&format!("caps{index}"));
            let before = LayoutState::new(&scene);
            let prepared = picture(
                &mut scene,
                &mut budget,
                &before,
                fixtures::capped_step(count, fixture),
            );
            assert_eq!(scene.usage().objects, count);
            assert_eq!(prepared.after.items().len(), count);
            retained.push((scene, prepared));
        }
        assert_eq!(budget.usage().scenes, 4);
        assert_eq!(budget.usage().objects, 1500);
        assert_eq!(retained.len(), 4);
    }
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
    assert!(bounds(&second.after, "left").max().y < bounds(&second.after, "right").origin.y);
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
        bounds(&second.after, "sibling").origin.y,
        cluster.max().y + 46.0
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
    )
    .expect("recovered cluster");
    assert_eq!(detached.items().len(), 4);
    assert_eq!(detached.items(), recovered.items());
    for (name, expected) in survivors.into_iter().zip(expected) {
        assert_eq!(bounds(&detached, name), expected);
        assert_eq!(bounds(&recovered, name), expected);
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
