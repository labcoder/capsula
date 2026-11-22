//! Cluster-local visual sizing and many-cluster preparation boundaries.
use kumochi_scene::{
    Admission, ErrorCode, Frame, GlobalBudget, Record, SceneState, SceneView,
    display::Prim,
    geom::{Rect, Vec2},
    layout::{LayoutState, PrepareError, PrepareInput, SurfaceGeometry, prepare, recover},
    parse_record,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[path = "support/scene.rs"]
mod scene_support;
use scene_support::{Measure, id, new_scene, prepared};

fn bounds(layout: &LayoutState, name: &str) -> Rect {
    layout
        .bounds(&id(name))
        .expect("safe bounds")
        .expect("object")
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

fn siblings(contents: Vec<Value>) -> Value {
    let mut put = vec![
        json!({"diagram":{"id":"outer","dir":"right"}}),
        json!({"diagram":{"id":"left","in":"outer"}}),
        json!({"node":{"id":"a","in":"left","label":"A"}}),
        json!({"diagram":{"id":"right","in":"outer"}}),
        json!({"node":{"id":"b","in":"right","label":"B"}}),
    ];
    put.extend(contents);
    json!({"id":"contents","put":put})
}

#[test]
fn absolute_local_geometry_sizes_a_cluster_before_its_sibling() {
    for visual in [
        json!({"line":{"id":"extra","in":"left","from":[-20,-10],"to":[220,230]}}),
        json!({"arrow":{"id":"extra","in":"left","from":[-20,-10],"to":[220,230]}}),
        json!({"shape":{"id":"extra","in":"left","kind":"rect","box":[-20,-10,240,240]}}),
        json!({"path":{"id":"extra","in":"left","d":"M-20 -10L220 230"}}),
    ] {
        let mut scene = new_scene("local");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            siblings(vec![visual]),
        );
        assert_rect(bounds(&picture.after, "left"), [60.0, 32.0, 240.0, 240.0]);
        assert_rect(bounds(&picture.after, "right"), [140.0, 334.0, 80.0, 40.0]);
    }
}

#[test]
fn local_text_and_reverse_order_anchor_dependencies_size_before_ranking() {
    let mut scene = new_scene("text");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        siblings(vec![
            json!({"text":{"id":"tag","in":"left","at":{"ref":"shape"},"text":"LOCAL"}}),
            json!({"shape":{"id":"shape","in":"left","kind":"rect","at":{"ref":"a"},"size":[240,100]}}),
        ]),
    );
    assert_rect(bounds(&picture.after, "left"), [60.0, 32.0, 240.0, 100.0]);
    assert_rect(bounds(&picture.after, "right"), [140.0, 194.0, 80.0, 40.0]);
    assert_rect(bounds(&picture.after, "tag"), [160.0, 77.0, 40.0, 10.0]);

    let mut scene = new_scene("talltext");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        siblings(vec![
            json!({"text":{"id":"tag","in":"left","at":[180,110],"text":"L"}}),
        ]),
    );
    assert_rect(bounds(&picture.after, "left"), [140.0, 32.0, 80.0, 99.0]);
    assert_rect(bounds(&picture.after, "right"), [140.0, 193.0, 80.0, 40.0]);
}

#[test]
fn local_highlights_and_callouts_contribute_their_measured_bounds() {
    for (visual, left, right, extra) in [
        (
            json!({"highlight":{"id":"extra","in":"left","target":{"ref":"a"},"pad":30}}),
            [110.0, 32.0, 140.0, 100.0],
            [140.0, 194.0, 80.0, 40.0],
            [110.0, 32.0, 140.0, 100.0],
        ),
        (
            json!({"callout":{"id":"extra","in":"left","target":{"ref":"a"},"at":[180,110],"text":"why"}}),
            [140.0, 32.0, 80.0, 108.0],
            [140.0, 202.0, 80.0, 40.0],
            [156.0, 68.0, 48.0, 72.0],
        ),
    ] {
        let mut scene = new_scene("annotations");
        let before = LayoutState::new(&scene);
        let picture = prepared(
            &mut scene,
            &mut GlobalBudget::default(),
            &before,
            siblings(vec![visual]),
        );
        assert_rect(bounds(&picture.after, "left"), left);
        assert_rect(bounds(&picture.after, "right"), right);
        assert_rect(bounds(&picture.after, "extra"), extra);
    }
}

#[test]
fn outside_sibling_and_self_anchors_still_draw_without_ranking_the_cluster() {
    for (right_first, a_y, b_y) in [(false, 32.0, 134.0), (true, 134.0, 32.0)] {
        let mut scene = new_scene("late");
        let before = LayoutState::new(&scene);
        let mut value = siblings(vec![
            json!({"diagram":{"id":"outside","box":[0,0,10,10]}}),
            json!({"text":{"id":"outside_tag","in":"left","at":{"ref":"outside"},"text":"O"}}),
            json!({"text":{"id":"sibling_tag","in":"left","at":{"ref":"b"},"text":"S"}}),
            json!({"text":{"id":"self_tag","in":"left","at":{"ref":"left"},"text":"C"}}),
            json!({"text":{"id":"ancestor_tag","in":"left","at":{"ref":"outer"},"text":"A"}}),
            json!({"line":{"id":"cross","in":"left","from":{"ref":"a"},"to":{"ref":"b"}}}),
        ]);
        if right_first {
            let put = value["put"].as_array_mut().expect("operations");
            put.swap(1, 3);
            put.swap(2, 4);
        }
        let picture = prepared(&mut scene, &mut GlobalBudget::default(), &before, value);
        assert_rect(bounds(&picture.after, "a"), [140.0, a_y, 80.0, 40.0]);
        assert_rect(bounds(&picture.after, "b"), [140.0, b_y, 80.0, 40.0]);
        assert_rect(bounds(&picture.after, "outside_tag"), [1.0, 0.0, 8.0, 10.0]);
        assert_rect(
            bounds(&picture.after, "sibling_tag"),
            [176.0, b_y + 15.0, 8.0, 10.0],
        );
        assert_rect(
            bounds(&picture.after, "self_tag"),
            [176.0, a_y + 15.0, 8.0, 10.0],
        );
        assert_rect(
            bounds(&picture.after, "ancestor_tag"),
            [176.0, 98.0, 8.0, 10.0],
        );
        assert_rect(bounds(&picture.after, "cross"), [180.0, 52.0, 0.0, 102.0]);
        let view = SceneView::new(&picture.after, 1);
        let mut frame = Frame::new();
        view.frame(0, &mut frame).expect("resting frame");
        for (name, expected) in [
            ("outside_tag", "O"),
            ("sibling_tag", "S"),
            ("self_tag", "C"),
            ("ancestor_tag", "A"),
        ] {
            let handle = picture.after.handle(&id(name)).expect("retained handle");
            let item = frame
                .items()
                .iter()
                .find(|item| item.handle == handle)
                .expect("drawn item");
            assert!(
                item.primitives.iter().any(
                    |prim| matches!(prim, Prim::Text { text, .. } if text.as_str() == expected)
                )
            );
        }
        assert_route(
            &picture.after,
            &frame,
            "cross",
            bounds(&picture.after, "a").center(),
            bounds(&picture.after, "b").center(),
        );
    }
}

fn assert_route(layout: &LayoutState, frame: &Frame, name: &str, from: Vec2, to: Vec2) {
    let handle = layout.handle(&id(name)).expect("line handle");
    let item = frame
        .items()
        .iter()
        .find(|item| item.handle == handle)
        .expect("painted line");
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
            points.first().zip(points.last()).is_some_and(|(a, b)| {
                item.transform.point(*a) == from && item.transform.point(*b) == to
            })
        }),
        "{name} connects the world endpoints {from:?} and {to:?}"
    );
}

#[test]
fn descendant_anchors_cancel_nested_and_already_ranked_cluster_transforms() {
    let mut scene = new_scene("descendants");
    let mut budget = GlobalBudget::default();
    let before = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &before,
        json!({"id":"nested","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"left","in":"outer"}},
            {"diagram":{"id":"inner","in":"left"}},
            {"node":{"id":"a","in":"inner","label":"A"}},
            {"node":{"id":"c","in":"left","label":"C"}},
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"b","in":"right","label":"B"}},
            {"shape":{"id":"box","in":"left","kind":"rect","at":{"ref":"a"},"size":[240,100]}}
        ]}),
    );
    assert_rect(bounds(&first.after, "left"), [60.0, 32.0, 240.0, 156.0]);
    assert_rect(bounds(&first.after, "a"), [140.0, 62.0, 80.0, 40.0]);
    assert_rect(bounds(&first.after, "box"), [60.0, 32.0, 240.0, 100.0]);
    assert_rect(bounds(&first.after, "right"), [140.0, 250.0, 80.0, 40.0]);
    let second = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"more","put":[
            {"node":{"id":"d","in":"left","label":"D"}},
            {"text":{"id":"tag","in":"left","at":{"ref":"a"},"text":"LOCAL"}}
        ]}),
    );
    assert_rect(bounds(&second.after, "a"), [140.0, 62.0, 80.0, 40.0]);
    assert_rect(bounds(&second.after, "tag"), [160.0, 77.0, 40.0, 10.0]);
    assert_rect(bounds(&second.after, "b"), [140.0, 250.0, 80.0, 40.0]);
    let third = prepared(
        &mut scene,
        &mut budget,
        &second.after,
        json!({"id":"late_only","put":[
            {"text":{"id":"tag2","in":"left","at":{"ref":"a"},"text":"LOCAL"}}
        ]}),
    );
    let (_, recovered) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("recovered anchors");
    for layout in [&third.after, &recovered] {
        for (name, expected) in [
            ("a", [140.0, 62.0, 80.0, 40.0]),
            ("b", [140.0, 250.0, 80.0, 40.0]),
            ("box", [60.0, 32.0, 240.0, 100.0]),
            ("tag", [160.0, 77.0, 40.0, 10.0]),
            ("tag2", [160.0, 77.0, 40.0, 10.0]),
        ] {
            assert_rect(bounds(layout, name), expected);
            assert_eq!(
                layout.handle(&id(name)),
                Some(scene.handle(&id(name)).expect("committed identity"))
            );
        }
        let items = layout.items();
        for name in ["tag", "tag2"] {
            let handle = layout.handle(&id(name)).expect("tag handle");
            let item = items
                .iter()
                .find(|item| item.handle == handle)
                .expect("tag");
            assert!(
                item.primitives.iter().any(
                    |prim| matches!(prim, Prim::Text { text, .. } if text.as_str() == "LOCAL")
                )
            );
        }
    }
}

#[test]
fn anchors_crossing_a_boxed_diagram_follow_its_resting_glide() {
    let mut scene = new_scene("boxed_glide");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"glide","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"left","in":"outer"}},
            {"diagram":{"id":"boxed","in":"left","dir":"right","box":[0,0,100,60]}},
            {"node":{"id":"first","in":"boxed","label":"A"}},
            {"node":{"id":"second","in":"boxed","label":"B"}},
            {"edge":{"id":"internal","in":"boxed","from":"first","to":"second"}},
            {"node":{"id":"local","in":"left","label":"C"}},
            {"text":{"id":"tag","in":"left","at":{"ref":"second"},"text":"T"}},
            {"text":{"id":"box_tag","in":"left","at":{"ref":"boxed"},"text":"Q"}},
            {"edge":{"id":"cross","in":"left","from":"second","to":"local"}}
        ]}),
    );
    let node = bounds(&picture.after, "second");
    let local = bounds(&picture.after, "local");
    assert_rect(node, [90.0, 48.0, 80.0, 40.0]);
    assert_rect(bounds(&picture.after, "tag"), [126.0, 63.0, 8.0, 10.0]);
    assert_rect(bounds(&picture.after, "boxed"), [-62.0, 48.0, 232.0, 40.0]);
    assert_rect(bounds(&picture.after, "box_tag"), [50.0, 63.0, 8.0, 10.0]);
    let view = SceneView::new(&picture.after, 1);
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("resting frame");
    assert_route(
        &picture.after,
        &frame,
        "cross",
        Vec2::new(node.max().x, node.center().y),
        Vec2::new(local.origin.x, local.center().y),
    );
}

fn input(value: &Value) -> (SceneState, GlobalBudget, PrepareInput) {
    let mut scene = new_scene("many");
    let before = LayoutState::new(&scene);
    let mut budget = GlobalBudget::default();
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let Admission::New(candidate) = scene.admit(&step, &budget).expect("admission") else {
        panic!("new step");
    };
    let committed = scene.commit(candidate, &mut budget).expect("commit");
    let input = PrepareInput::new(
        committed.preparation,
        before,
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("input");
    (scene, budget, input)
}

#[test]
fn many_clusters_admit_at_the_object_cap_and_fail_only_at_the_geometry_boundary() {
    // shortcut: flat 299-cluster drawings reject until A9 adds scale-to-fit.
    for direction in ["down", "right"] {
        let (_, budget, input) = input(&flat_at_cap(direction));
        assert_eq!(budget.usage().objects, 600);
        let error = prepare(input, &mut Measure, &AtomicBool::new(false))
            .expect_err("unscaled geometry exceeds canvas");
        let PrepareError::Layout(error) = error else {
            panic!("layout failure");
        };
        assert_eq!(error.code, ErrorCode::OutOfRange);
    }
    let (scene, budget, input) = input(&nested_step());
    assert_eq!(budget.usage().objects, 600);
    let picture =
        prepare(input, &mut Measure, &AtomicBool::new(false)).expect("nested cap picture");
    assert_eq!(picture.after.items().len(), 600);
    assert_eq!(scene.object_count(), 600);
}

#[test]
fn reverse_anchor_chain_prepares_all_locally_owned_texts() {
    let (_, budget, input) = input(&reverse_chains());
    assert_eq!(budget.usage().objects, 403);
    let picture = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("reverse chain");
    assert_eq!(picture.after.items().len(), 403);
    for index in 0..400 {
        assert_rect(
            bounds(&picture.after, &format!("t{index}")),
            [176.0, 47.0, 8.0, 10.0],
        );
    }
}

#[test]
#[ignore = "standalone release executable on quiet Rez under measuring.lock"]
fn prepare_many_clusters() {
    use std::{hint::black_box, time::Instant};

    for (name, fixture, objects, rejects) in [
        ("flat16", flat_step(16), 33, false),
        ("flat32", flat_step(32), 65, false),
        ("flat64", flat_step(64), 129, false),
        ("flat128", flat_step(128), 257, false),
        ("nested15x19", nested_step(), 600, false),
        ("flat299down", flat_at_cap("down"), 600, true),
        ("flat299right", flat_at_cap("right"), 600, true),
        ("reverse40x10", reverse_chains(), 403, false),
    ] {
        let mut samples = Vec::with_capacity(64);
        for iteration in 0..69 {
            let (_scene, budget, input) = input(&fixture);
            assert_eq!(budget.usage().objects, objects);
            let cancel = AtomicBool::new(false);
            let start = Instant::now();
            let result = prepare(black_box(input), &mut Measure, &cancel);
            let elapsed = start.elapsed().as_secs_f64() * 1e6;
            if rejects {
                let PrepareError::Layout(error) = result.expect_err("coordinate boundary") else {
                    panic!("layout failure");
                };
                assert_eq!(error.code, ErrorCode::OutOfRange);
            } else {
                assert_eq!(result.expect("picture").after.items().len(), objects);
            }
            if iteration >= 5 {
                samples.push(elapsed);
            }
        }
        println!("workload={name} samples=64");
        for (index, elapsed) in samples.into_iter().enumerate() {
            println!("sample {index} {elapsed:.3}");
        }
    }
}
fn flat_step(clusters: usize) -> Value {
    let mut put = Vec::with_capacity(1 + 2 * clusters);
    put.push(json!({"diagram":{"id":"root","dir":"down"}}));
    for index in 0..clusters {
        put.push(json!({"diagram":{"id":format!("g{index}"),"in":"root"}}));
        put.push(json!({"node":{
            "id":format!("n{index}"),"in":format!("g{index}"),"label":"N"
        }}));
    }
    let value = json!({"id":"flat","put":put});
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}

fn flat_at_cap(direction: &str) -> Value {
    assert!(
        ["down", "right"].contains(&direction),
        "supported flow direction"
    );
    let mut value = flat_step(299);
    let put = value["put"]
        .as_array_mut()
        .expect("flat fixture operations");
    put[0]["diagram"]["dir"] = json!(direction);
    put.push(json!({"line":{"id":"padding","from":[0,0],"to":[1,1]}}));
    assert_eq!(put.len(), 600, "299 clusters and nodes plus root and line");
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}

fn nested_step() -> Value {
    let mut put = Vec::with_capacity(600);
    put.push(json!({"diagram":{"id":"root","dir":"down"}}));
    for mid in 0..15 {
        put.push(json!({"diagram":{
            "id":format!("m{mid}"),"in":"root","dir":"right"
        }}));
        for leaf in 0..19 {
            put.push(json!({"diagram":{
                "id":format!("g{mid}_{leaf}"),"in":format!("m{mid}")
            }}));
            put.push(json!({"node":{
                "id":format!("n{mid}_{leaf}"),"in":format!("g{mid}_{leaf}"),"label":"N"
            }}));
        }
    }
    for index in 0..14 {
        put.push(json!({"line":{"id":format!("p{index}"),"from":[0,0],"to":[1,1]}}));
    }
    assert_eq!(put.len(), 600, "301 diagrams, 285 nodes and 14 lines");
    let value = json!({"id":"nested","put":put});
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}

fn reverse_chains() -> Value {
    let mut put = vec![
        json!({"diagram":{"id":"root"}}),
        json!({"diagram":{"id":"g","in":"root"}}),
        json!({"node":{"id":"a","in":"g","label":"A"}}),
    ];
    for index in 0..400 {
        let target = if index >= 360 {
            "a".to_owned()
        } else {
            format!("t{}", index + 40)
        };
        put.push(json!({"text":{
            "id":format!("t{index}"),"in":"g","at":{"ref":target},"text":"x"
        }}));
    }
    let value = json!({"id":"reverse","put":put});
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}
