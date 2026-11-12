//! `set.behind`: admission, paint order, a removed anchor, recovery and stored records.
#[path = "support/scene.rs"]
mod scene_support;

use kumochi_scene::{
    ErrorCode, Frame, GlobalBudget, Handle, Record, SceneEvent, SceneState, SceneView, Step,
    layout::{LayoutState, SurfaceGeometry, recover},
    parse_record,
    theme::Theme,
};
use scene_support::{Measure, id, new_scene, prepared};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn circle(name: &str) -> Value {
    json!({"shape":{"id":name,"kind":"circle","at":[50,50],"r":20,"fill":"#ffffff","enter":"none"}})
}

fn typed(value: Value) -> Step {
    let bytes = serde_json::to_vec(&json!({ "step": value })).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    step
}

/// Prepares and activates each step on one view.
fn drawn(steps: Vec<Value>) -> (SceneState, GlobalBudget, SceneView) {
    let mut scene = new_scene("behind");
    let mut budget = GlobalBudget::default();
    let mut view = SceneView::new(&LayoutState::new(&scene), scene.activated());
    for (now, value) in (0..).step_by(100).zip(steps) {
        let step = prepared(&mut scene, &mut budget, view.layout(), value);
        view.activate(step, now, 0, &mut Frame::new())
            .expect("fresh activation");
    }
    (scene, budget, view)
}

/// The named objects in the order they paint, bottom first, at rest.
fn painted(view: &SceneView, names: &[&str]) -> Vec<String> {
    let mut frame = Frame::new();
    view.frame(120_000, &mut frame).expect("fresh frame");
    let named: Vec<(Handle, &str)> = names
        .iter()
        .filter_map(|name| Some((view.layout().handle(&id(name))?, *name)))
        .collect();
    frame
        .items()
        .iter()
        .filter_map(|item| named.iter().find(|(handle, _)| *handle == item.handle))
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

const ALL: &[&str] = &["sky", "c1", "c2", "c3", "sun", "moon", "star"];

fn cloud() -> Value {
    json!({"id":"cloud","put":[
        {"shape":{"id":"sky","kind":"rect","box":[0,0,200,100],"fill":"#88ccff","enter":"none"}},
        circle("c1"),
        circle("c2")
    ]})
}

#[test]
fn an_object_put_behind_another_paints_just_below_it() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],"set":[{"target":"sun","behind":"c1"}]}),
        json!({"id":"warm","set":[{"target":"sun","color":"#ffcc00"}]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["sky", "sun", "c1", "c2"],
        "a later set on the target keeps its place"
    );
}

#[test]
fn chains_and_later_objects_behind_one_anchor_keep_a_deterministic_order() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun"),circle("moon")],
            "set":[{"target":"sun","behind":"c1"},{"target":"moon","behind":"sun"}]}),
        json!({"id":"star","put":[circle("star")],"set":[{"target":"star","behind":"c1"}]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["sky", "moon", "sun", "star", "c1", "c2"],
        "deeper chains first; at one depth, the newer object paints higher"
    );
}

#[test]
fn an_existing_object_moves_behind_an_earlier_one() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"back","set":[{"target":"c2","behind":"sky"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["c2", "sky", "c1"]);
}

#[test]
fn a_removed_anchor_leaves_its_dependant_where_it_painted_and_recovery_agrees() {
    let (scene, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],"set":[{"target":"sun","behind":"c1"}]}),
        json!({"id":"drop","remove":["c1"]}),
        json!({"id":"star","put":[circle("star"), circle("c3")],"set":[{"target":"c3","behind":"sun"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["sky", "c3", "sun", "c2", "star"]);
    let (state, layout) = recover(
        scene.header().clone(),
        scene.recovery_log(),
        SurfaceGeometry::default(),
        &Arc::new(Theme::default()),
        &mut Measure,
        &AtomicBool::new(false),
        None,
    )
    .expect("replay");
    let recovered = SceneView::new(&layout, state.activated());
    assert_eq!(painted(&recovered, ALL), ["sky", "c3", "sun", "c2", "star"]);
}

fn refused(steps: &[Value], last: Value, code: ErrorCode, path: &str) {
    let (scene, budget, _) = drawn(steps.to_vec());
    let revision = scene.revision();
    let error = scene.admit(&typed(last), &budget).expect_err("refused");
    assert_eq!((error.code, error.path.as_str()), (code, path));
    assert_eq!(scene.revision(), revision, "a refusal commits nothing");
}

#[test]
fn a_link_that_would_close_a_cycle_is_refused_in_the_step_that_closes_it() {
    let cloud = cloud();
    refused(
        std::slice::from_ref(&cloud),
        json!({"id":"self","set":[{"target":"c1","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    let first = json!({"id":"one","set":[{"target":"c1","behind":"c2"}]});
    refused(
        &[cloud.clone(), first.clone()],
        json!({"id":"two","set":[{"target":"c2","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    refused(
        &[
            cloud.clone(),
            first,
            json!({"id":"two","set":[{"target":"c2","behind":"sky"}]}),
        ],
        json!({"id":"three","set":[{"target":"sky","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    refused(
        std::slice::from_ref(&cloud),
        json!({"id":"both","set":[{"target":"c1","behind":"c2"},{"target":"c2","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[1].behind",
    );
}

#[test]
fn a_chain_ends_at_a_removed_anchor() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"one","set":[{"target":"c1","behind":"c2"}]}),
        json!({"id":"drop","remove":["c2"]}),
        json!({"id":"two","set":[{"target":"sky","behind":"c1"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["sky", "c1"]);
}

#[test]
fn unknown_removed_container_and_three_dimensional_links_are_refused() {
    let cloud = cloud();
    let base = std::slice::from_ref(&cloud);
    refused(
        base,
        json!({"id":"x","set":[{"target":"c1","behind":"nothing"}]}),
        ErrorCode::UnknownRef,
        "set[0].behind",
    );
    refused(
        base,
        json!({"id":"x","remove":["c2"],"set":[{"target":"c1","behind":"c2"}]}),
        ErrorCode::RemovedInStep,
        "set[0].behind",
    );
    refused(
        base,
        json!({"id":"x","put":[{"diagram":{"id":"d"}}],"set":[{"target":"d","behind":"c1"}]}),
        ErrorCode::KindChanged,
        "set[0].behind",
    );
}

#[test]
fn a_stored_step_written_before_behind_existed_still_decodes() {
    // A step event exactly as the format before `behind` serialized it into the store.
    let stored = r##"{"step":{"revision":3,"step":{"id":"red","say":null,"put":[],"set":[{"target":"c1","text":null,"label":null,"color":"#ff0000","cells":null,"data":null,"emphasis":null,"info":null}],"remove":[],"start":"speech"}}}"##;
    let event: SceneEvent = serde_json::from_str(stored).expect("old stored step decodes");
    let mut expected: Value = serde_json::from_str(stored).expect("JSON");
    expected["step"]["step"]["set"][0]["behind"] = Value::Null;
    assert_eq!(
        serde_json::to_value(&event).expect("JSON"),
        expected,
        "it decodes with no behind and nothing else changed"
    );
}
