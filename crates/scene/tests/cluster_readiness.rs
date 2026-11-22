//! Cluster readiness: contents anchored outside their cluster wait for its ranking without
//! holding back the local contents that follow them.
use kumochi_scene::{
    Admission, ErrorCode, GlobalBudget, Record, SceneState,
    geom::Rect,
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
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

fn input(value: &Value) -> (SceneState, GlobalBudget, PrepareInput) {
    let mut scene = new_scene("readiness");
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

fn text(index: usize, at: &Value) -> Value {
    json!({"text":{"id":index.to_string(),"in":"l","at":at,"text":""}})
}

#[derive(Clone, Copy)]
enum SourceOrder {
    FarFirst,
    NearFirst,
}

/// Texts 0 to 279 anchor to node `d`, `chain` clusters down another branch, and texts 280 to
/// 559 sit at a fixed point in cluster `l`; `d` is at object depth `chain + 2` and the anchored
/// texts one deeper, so 13 puts them at the depth limit.
fn far_anchors(chain: usize, order: SourceOrder) -> Value {
    // One-letter and numeric ids and empty texts keep 560 texts inside the record byte cap.
    let mut put = vec![
        json!({"diagram":{"id":"outer","dir":"right"}}),
        json!({"diagram":{"id":"l","in":"outer"}}),
        json!({"node":{"id":"a","in":"l","label":"A"}}),
    ];
    let mut parent = "outer".to_owned();
    for level in 1..=chain {
        let cluster = format!("r{level}");
        put.push(json!({"diagram":{"id":cluster,"in":parent}}));
        parent = cluster;
    }
    put.push(json!({"node":{"id":"d","in":parent,"label":"B"}}));
    let far = (0..280).map(|index| text(index, &json!({"ref":"d"})));
    let near = (280..560).map(|index| text(index, &json!([180, 110])));
    if matches!(order, SourceOrder::FarFirst) {
        put.extend(far);
        put.extend(near);
    } else {
        put.extend(near);
        put.extend(far);
    }
    assert_eq!(put.len(), chain + 564, "the chain plus 564 objects");
    let value = json!({"id":"deep","put":put});
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}

#[test]
fn outside_anchors_ahead_of_a_local_dependency_stay_late_without_blocking_it() {
    let mut scene = new_scene("order");
    let before = LayoutState::new(&scene);
    let picture = prepared(
        &mut scene,
        &mut GlobalBudget::default(),
        &before,
        json!({"id":"order","put":[
            {"diagram":{"id":"outer","dir":"right"}},
            {"diagram":{"id":"left","in":"outer"}},
            {"node":{"id":"a","in":"left","label":"A"}},
            {"diagram":{"id":"right","in":"outer"}},
            {"node":{"id":"b","in":"right","label":"B"}},
            {"text":{"id":"hop","in":"left","at":{"ref":"far"},"text":"x"}},
            {"text":{"id":"far","in":"left","at":{"ref":"b"},"text":"x"}},
            {"shape":{"id":"box","in":"left","kind":"rect","at":{"ref":"near"},"size":[240,100]}},
            {"text":{"id":"near","in":"left","at":[180,110],"text":"x"}}
        ]}),
    );
    assert_rect(bounds(&picture.after, "near"), [176.0, 121.0, 8.0, 10.0]);
    assert_rect(bounds(&picture.after, "box"), [60.0, 76.0, 240.0, 100.0]);
    assert_rect(bounds(&picture.after, "b"), [140.0, 238.0, 80.0, 40.0]);
    assert_rect(bounds(&picture.after, "far"), [176.0, 253.0, 8.0, 10.0]);
    assert_rect(bounds(&picture.after, "hop"), [176.0, 253.0, 8.0, 10.0]);
    assert_rect(bounds(&picture.after, "left"), [60.0, 32.0, 240.0, 231.0]);
}

#[test]
fn depth_limit_anchors_stay_late_in_either_source_order() {
    for order in [SourceOrder::FarFirst, SourceOrder::NearFirst] {
        let (_, budget, input) = input(&far_anchors(13, order));
        assert_eq!(budget.usage().objects, 577);
        let picture = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("picture");
        assert_eq!(picture.after.items().len(), 577);
        assert_rect(bounds(&picture.after, "d"), [140.0, 193.0, 80.0, 40.0]);
        assert_rect(bounds(&picture.after, "l"), [140.0, 32.0, 80.0, 186.0]);
        for index in 0..280 {
            assert_rect(
                bounds(&picture.after, &index.to_string()),
                [180.0, 208.0, 0.0, 10.0],
            );
        }
        for index in 280..560 {
            assert_rect(
                bounds(&picture.after, &index.to_string()),
                [180.0, 121.0, 0.0, 10.0],
            );
        }
    }
}

#[test]
fn anchors_one_level_past_the_depth_limit_reject_at_admission() {
    let value = far_anchors(14, SourceOrder::FarFirst);
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    let scene = new_scene("too_deep");
    let result = scene.admit(&step, &GlobalBudget::default());
    assert_eq!(result.expect_err("depth 17").code, ErrorCode::TooDeep);
}
