//! `set.behind`: admission, paint order, removed and cut anchors, recovery and stored records.
#[path = "support/scene.rs"]
mod scene_support;

use kumochi_scene::{
    ErrorCode, Frame, GlobalBudget, Handle, Record, SceneEvent, SceneState, SceneView, Step,
    StopCut,
    layout::{LayoutState, SurfaceGeometry, recover},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use scene_support::{Measure, id, new_scene, prepared};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn circle(name: &str) -> Value {
    json!({"shape":{"id":name,"kind":"circle","at":[50,50],"r":20,"fill":"#ffffff","enter":"none"}})
}

fn typed(value: &Value) -> Step {
    let bytes = serde_json::to_vec(&json!({ "step": value })).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    step
}

/// Commits, prepares and activates one step on `view`.
fn activate(scene: &mut SceneState, budget: &mut GlobalBudget, view: &mut SceneView, value: Value) {
    let now = u64::from(scene.revision()) * 100;
    let step = prepared(scene, budget, view.layout(), value);
    view.activate(step, now, 0, &mut Frame::new())
        .expect("fresh activation");
}

/// Prepares and activates each step on one view.
fn drawn(steps: Vec<Value>) -> (SceneState, GlobalBudget, SceneView) {
    let mut scene = new_scene("behind");
    let mut budget = GlobalBudget::default();
    let mut view = SceneView::new(&LayoutState::new(&scene), scene.activated());
    for value in steps {
        activate(&mut scene, &mut budget, &mut view, value);
    }
    (scene, budget, view)
}

/// The scene rebuilt from its recovery log, as a restart shows it.
fn recovered(scene: &SceneState) -> SceneView {
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
    SceneView::new(&layout, state.activated())
}

/// Stops through `activated`, keeping every object on `view` except `dropped`, and returns the
/// live view of the cut.
fn cut(
    scene: &mut SceneState,
    budget: &mut GlobalBudget,
    view: &SceneView,
    activated: u32,
    dropped: &[&str],
) -> SceneView {
    let dropped: Vec<Handle> = dropped
        .iter()
        .map(|name| view.layout().handle(&id(name)).expect("dropped object"))
        .collect();
    let revealed = view
        .layout()
        .items()
        .iter()
        .map(|item| item.handle)
        .filter(|handle| !dropped.contains(handle))
        .collect::<Vec<_>>();
    scene
        .stop(
            &StopCut {
                activated,
                revealed: Bounded::new(revealed).expect("bounded cut"),
            },
            budget,
        )
        .expect("effective cut");
    let live = view.layout().cut(scene).expect("cut layout");
    SceneView::new(&live, scene.activated())
}

/// The named objects in the order they paint, bottom first, at rest.
fn painted(view: &SceneView, names: &[&str]) -> Vec<String> {
    let mut frame = Frame::new();
    view.frame(120_000, &mut frame).expect("fresh frame");
    let handles: Vec<(Handle, &str)> = names
        .iter()
        .filter_map(|name| Some((view.layout().handle(&id(name))?, *name)))
        .collect();
    frame
        .items()
        .iter()
        .filter_map(|item| handles.iter().find(|(handle, _)| *handle == item.handle))
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

const ALL: &[&str] = &["sky", "c1", "c2", "c3", "sun", "moon", "star", "comet"];

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
fn a_chain_paints_as_one_group_below_its_anchor() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun"),circle("moon")],
            "set":[{"target":"sun","behind":"c1"},{"target":"moon","behind":"sun"}]}),
        json!({"id":"star","put":[circle("star")],"set":[{"target":"star","behind":"c1"}]}),
        json!({"id":"comet","put":[circle("comet")],"set":[{"target":"comet","behind":"star"}]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["sky", "moon", "sun", "comet", "star", "c1", "c2"],
        "everything behind sun paints with it, and comet sits just below star alone"
    );
}

#[test]
fn an_anchor_s_dependants_keep_handle_order() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],"set":[{"target":"sun","behind":"c2"}]}),
        json!({"id":"older","set":[{"target":"c1","behind":"c2"}]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["sky", "c1", "sun", "c2"],
        "the older c1 paints below the newer sun, whatever order the links came in"
    );
}

#[test]
fn a_dependant_follows_its_anchor_when_the_anchor_moves_later() {
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun"),circle("moon")],"set":[{"target":"moon","behind":"sun"}]}),
        json!({"id":"move","set":[{"target":"sun","behind":"c1"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["sky", "moon", "sun", "c1", "c2"]);
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],"set":[{"target":"sun","behind":"c2"}]}),
        json!({"id":"move","set":[{"target":"c2","behind":"c1"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["sky", "sun", "c2", "c1"]);
}

#[test]
fn the_same_links_paint_the_same_whatever_order_they_were_set_in() {
    let put = || json!([circle("sun"), circle("moon")]);
    let moon = json!({"target":"moon","behind":"sun"});
    let sun = json!({"target":"sun","behind":"c2"});
    let expected = ["sky", "c1", "moon", "sun", "c2"];
    for set in [json!([moon, sun]), json!([sun, moon])] {
        let (_, _, view) = drawn(vec![cloud(), json!({"id":"one","put":put(),"set":set})]);
        assert_eq!(painted(&view, ALL), expected, "one step");
    }
    let (_, _, view) = drawn(vec![
        cloud(),
        json!({"id":"one","put":put(),"set":[moon]}),
        json!({"id":"two","set":[sun]}),
    ]);
    assert_eq!(painted(&view, ALL), expected, "across steps");
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
    assert_eq!(
        painted(&recovered(&scene), ALL),
        ["sky", "c3", "sun", "c2", "star"]
    );
}

#[test]
fn a_removed_anchor_keeps_its_own_link_for_its_dependants() {
    let (scene, _, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun"),circle("moon")],
            "set":[{"target":"sun","behind":"c1"},{"target":"moon","behind":"sun"}]}),
        json!({"id":"drop","remove":["sun"]}),
        json!({"id":"move","set":[{"target":"c1","behind":"sky"}]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["moon", "c1", "sky", "c2"],
        "moon is behind c1 now, so it moves with c1"
    );
    assert_eq!(
        painted(&recovered(&scene), ALL),
        ["moon", "c1", "sky", "c2"]
    );
}

#[test]
fn removing_an_anchor_keeps_its_dependants_where_they_were() {
    let tree = json!({"id":"sun","put":[circle("sun")],"set":[
        {"target":"sun","behind":"c2"},{"target":"c1","behind":"c2"},{"target":"sky","behind":"sun"}]});
    let (scene, _, view) = drawn(vec![
        cloud(),
        tree.clone(),
        json!({"id":"drop","remove":["sun"]}),
    ]);
    assert_eq!(
        painted(&view, ALL),
        ["c1", "sky", "c2"],
        "sky stays above c1"
    );
    assert_eq!(painted(&recovered(&scene), ALL), ["c1", "sky", "c2"]);
    let (scene, _, view) = drawn(vec![
        cloud(),
        tree,
        json!({"id":"root","remove":["c2"]}),
        json!({"id":"drop","remove":["sun"]}),
    ]);
    assert_eq!(painted(&view, ALL), ["c1", "sky"], "at the root too");
    assert_eq!(painted(&recovered(&scene), ALL), ["c1", "sky"]);
}

#[test]
fn nested_removed_anchors_keep_the_order_of_what_survives() {
    let names = ["a1", "a2", "b", "z"];
    let put = json!({"id":"put","put":[circle("a1"),circle("a2"),circle("b"),circle("z")],"set":[
        {"target":"a1","behind":"b"},{"target":"a2","behind":"b"},{"target":"z","behind":"a1"}]});
    let (scene, _, view) = drawn(vec![put.clone(), json!({"id":"one","remove":["a1"]})]);
    assert_eq!(painted(&view, &names), ["z", "a2", "b"]);
    assert_eq!(painted(&recovered(&scene), &names), ["z", "a2", "b"]);
    let (scene, _, view) = drawn(vec![
        put,
        json!({"id":"one","remove":["a1"]}),
        json!({"id":"two","remove":["b"]}),
    ]);
    assert_eq!(painted(&view, &names), ["z", "a2"]);
    assert_eq!(painted(&recovered(&scene), &names), ["z", "a2"]);
}

#[test]
fn removing_two_anchors_in_one_step_keeps_the_order_of_what_survives() {
    let names = ["a1", "a2", "b", "z"];
    let (scene, _, view) = drawn(vec![
        json!({"id":"put","put":[circle("a1"),circle("a2"),circle("b"),circle("z")],"set":[
            {"target":"a1","behind":"b"},{"target":"a2","behind":"b"},{"target":"z","behind":"a1"}]}),
        json!({"id":"both","remove":["a1","b"]}),
    ]);
    assert_eq!(painted(&view, &names), ["z", "a2"]);
    assert_eq!(painted(&recovered(&scene), &names), ["z", "a2"]);
}

#[test]
fn two_cuts_hold_dropped_anchors_and_drop_unshown_links_alike_live_and_recovered() {
    let (mut scene, mut budget, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],
            "set":[{"target":"sun","behind":"c2"},{"target":"c2","behind":"c1"}]}),
    ]);
    assert_eq!(painted(&view, ALL), ["sky", "sun", "c2", "c1"]);
    drop(prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"unshown","set":[{"target":"sky","behind":"sun"}]}),
    ));
    let mut view = cut(&mut scene, &mut budget, &view, 2, &["c2"]);
    assert_eq!(
        painted(&view, ALL),
        ["sky", "sun", "c1"],
        "sun stays under the held c2, behind c1, and the unshown link is gone"
    );
    assert_eq!(painted(&recovered(&scene), ALL), ["sky", "sun", "c1"]);
    activate(
        &mut scene,
        &mut budget,
        &mut view,
        json!({"id":"star","put":[circle("star")],
            "set":[{"target":"star","behind":"sun"},{"target":"sun","behind":"sky"}]}),
    );
    assert_eq!(painted(&view, ALL), ["star", "sun", "sky", "c1"]);
    let view = cut(&mut scene, &mut budget, &view, 4, &["sun"]);
    assert_eq!(painted(&view, ALL), ["star", "sky", "c1"]);
    assert_eq!(painted(&recovered(&scene), ALL), ["star", "sky", "c1"]);
}

#[test]
fn a_cut_that_restores_a_removed_anchor_restores_its_links() {
    let (mut scene, mut budget, view) = drawn(vec![
        cloud(),
        json!({"id":"sun","put":[circle("sun")],"set":[{"target":"sun","behind":"c1"}]}),
    ]);
    drop(prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"drop","remove":["c1"]}),
    ));
    let view = cut(&mut scene, &mut budget, &view, 2, &[]);
    assert_eq!(painted(&view, ALL), ["sky", "sun", "c1", "c2"]);
    assert_eq!(painted(&recovered(&scene), ALL), ["sky", "sun", "c1", "c2"]);
    let error = scene
        .admit(
            &typed(&json!({"id":"back","set":[{"target":"c1","behind":"sun"}]})),
            &budget,
        )
        .expect_err("a cycle through the restored anchor");
    assert_eq!(
        (error.code, error.path.as_str()),
        (ErrorCode::ReferenceCycle, "set[0].behind")
    );
}

fn refused(steps: &[Value], last: &Value, code: ErrorCode, path: &str) {
    let (scene, budget, _) = drawn(steps.to_vec());
    let error = scene.admit(&typed(last), &budget).expect_err("refused");
    assert_eq!(
        (error.code, error.path.as_str()),
        (code, path),
        "the refusal names its code and field"
    );
}

fn admitted(steps: &[Value], last: &Value) {
    let (scene, budget, _) = drawn(steps.to_vec());
    scene.admit(&typed(last), &budget).expect("admitted");
}

#[test]
fn a_link_that_would_close_a_cycle_is_refused_in_the_step_that_closes_it() {
    let cloud = cloud();
    refused(
        std::slice::from_ref(&cloud),
        &json!({"id":"self","set":[{"target":"c1","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    let first = json!({"id":"one","set":[{"target":"c1","behind":"c2"}]});
    refused(
        &[cloud.clone(), first.clone()],
        &json!({"id":"two","set":[{"target":"c2","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    refused(
        &[
            cloud.clone(),
            first,
            json!({"id":"two","set":[{"target":"c2","behind":"sky"}]}),
        ],
        &json!({"id":"three","set":[{"target":"sky","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    refused(
        std::slice::from_ref(&cloud),
        &json!({"id":"both","set":[{"target":"c1","behind":"c2"},{"target":"c2","behind":"c1"}]}),
        ErrorCode::ReferenceCycle,
        "set[1].behind",
    );
}

#[test]
fn a_cycle_through_a_removed_anchor_is_refused_in_one_step_or_two() {
    let linked = [
        cloud(),
        json!({"id":"sun","put":[circle("sun")],
            "set":[{"target":"sun","behind":"c1"},{"target":"c1","behind":"c2"}]}),
    ];
    refused(
        &linked,
        &json!({"id":"x","remove":["c1"],"set":[{"target":"c2","behind":"sun"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
    let mut removed = linked.to_vec();
    removed.push(json!({"id":"drop","remove":["c1"]}));
    refused(
        &removed,
        &json!({"id":"x","set":[{"target":"c2","behind":"sun"}]}),
        ErrorCode::ReferenceCycle,
        "set[0].behind",
    );
}

#[test]
fn a_re_pointed_target_s_old_link_no_longer_closes_a_cycle() {
    admitted(
        &[
            cloud(),
            json!({"id":"one","set":[{"target":"c1","behind":"c2"}]}),
            json!({"id":"two","set":[{"target":"c1","behind":"sky"}]}),
        ],
        &json!({"id":"three","set":[{"target":"c2","behind":"c1"}]}),
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

/// `count` circles named `{prefix}0` upwards, each behind the one before it.
fn chain(prefix: &str, count: usize) -> (Vec<Value>, Vec<Value>) {
    let name = |index: usize| format!("{prefix}{index}");
    let put = (0..count).map(|index| circle(&name(index))).collect();
    let set = (1..count)
        .map(|index| json!({"target":name(index),"behind":name(index - 1)}))
        .collect();
    (put, set)
}

#[test]
fn a_chain_may_hold_sixteen_links_but_not_seventeen() {
    let (put, set) = chain("o", 18);
    let sixteen = [json!({"id":"chain","put":put,"set":&set[..16]})];
    refused(
        &sixteen,
        &json!({"id":"more","set":[set[16]]}),
        ErrorCode::TooDeep,
        "set[0].behind",
    );
    let (a, a_links) = chain("a", 9);
    let (b, b_links) = chain("b", 9);
    let joined = |b_count: usize| {
        let put = [a.clone(), b.clone()].concat();
        let set = [&a_links[..], &b_links[..b_count]].concat();
        [json!({"id":"both","put":put,"set":set})]
    };
    let join = json!({"id":"join","set":[{"target":"b0","behind":"a8"}]});
    admitted(&joined(7), &join);
    refused(&joined(8), &join, ErrorCode::TooDeep, "set[0].behind");
}

#[test]
fn a_re_pointed_target_no_longer_counts_below_its_old_anchor() {
    let (a, a_links) = chain("a", 9);
    let (b, b_links) = chain("b", 9);
    let put = [a, b, vec![circle("p"), circle("q")]].concat();
    let mut set = [a_links, b_links].concat();
    set.push(json!({"target":"b0","behind":"p"}));
    admitted(
        &[json!({"id":"both","put":put,"set":set})],
        &json!({"id":"move","set":[{"target":"b0","behind":"q"},{"target":"p","behind":"a8"}]}),
    );
}

#[test]
fn a_removed_object_inside_a_chain_still_counts_towards_sixteen() {
    let (put, set) = chain("o", 18);
    let chain = [
        json!({"id":"chain","put":put,"set":&set[..16]}),
        json!({"id":"drop","remove":["o8"]}),
    ];
    admitted(
        &chain,
        &json!({"id":"same","set":[{"target":"o16","behind":"o15"}]}),
    );
    refused(
        &chain,
        &json!({"id":"more","set":[set[16]]}),
        ErrorCode::TooDeep,
        "set[0].behind",
    );
}

#[test]
fn a_removed_stack_with_nothing_live_behind_it_no_longer_counts() {
    let (mut put, set) = chain("o", 17);
    put.push(circle("x"));
    let removed: Vec<String> = (1..17).map(|index| format!("o{index}")).collect();
    admitted(
        &[
            json!({"id":"chain","put":put,"set":set}),
            json!({"id":"drop","remove":removed}),
        ],
        &json!({"id":"move","set":[{"target":"o0","behind":"x"}]}),
    );
}

#[test]
fn a_stack_removed_in_a_step_still_counts_in_that_step() {
    let (mut put, set) = chain("o", 17);
    put.push(circle("x"));
    let removed: Vec<String> = (1..17).map(|index| format!("o{index}")).collect();
    refused(
        &[json!({"id":"chain","put":put,"set":set})],
        &json!({"id":"move","remove":removed,"set":[{"target":"o0","behind":"x"}]}),
        ErrorCode::TooDeep,
        "set[0].behind",
    );
}

#[test]
fn a_removed_object_below_a_target_still_counts_towards_sixteen() {
    let link_base = |count: usize| {
        let (mut put, set) = chain("o", count);
        put.push(circle("x"));
        let removed: Vec<String> = (1..count - 1).map(|index| format!("o{index}")).collect();
        (
            [
                json!({"id":"chain","put":put,"set":set}),
                json!({"id":"drop","remove":removed}),
            ],
            json!({"id":"move","set":[{"target":"o0","behind":"x"}]}),
        )
    };
    let (steps, link) = link_base(16);
    admitted(&steps, &link);
    let (steps, link) = link_base(17);
    refused(&steps, &link, ErrorCode::TooDeep, "set[0].behind");
}

#[test]
fn links_stay_between_objects_with_the_same_parent() {
    let base = [cloud()];
    let diagram = |set: Value| {
        json!({"id":"x","put":[{"diagram":{"id":"d"}},
            {"node":{"id":"n","in":"d","label":"N"}},{"node":{"id":"m","in":"d","label":"M"}}],
            "set":set})
    };
    refused(
        &base,
        &diagram(json!([{"target":"n","behind":"c1"}])),
        ErrorCode::KindChanged,
        "set[0].behind",
    );
    refused(
        &base,
        &diagram(json!([{"target":"c1","behind":"n"}])),
        ErrorCode::KindChanged,
        "set[0].behind",
    );
    admitted(&base, &diagram(json!([{"target":"m","behind":"n"}])));
}

#[test]
fn unknown_removed_container_and_three_dimensional_links_are_refused() {
    let cloud = cloud();
    let base = std::slice::from_ref(&cloud);
    refused(
        base,
        &json!({"id":"x","set":[{"target":"c1","behind":"nothing"}]}),
        ErrorCode::UnknownRef,
        "set[0].behind",
    );
    refused(
        base,
        &json!({"id":"x","remove":["c2"],"set":[{"target":"c1","behind":"c2"}]}),
        ErrorCode::RemovedInStep,
        "set[0].behind",
    );
    refused(
        base,
        &json!({"id":"x","put":[{"diagram":{"id":"d"}}],"set":[{"target":"d","behind":"c1"}]}),
        ErrorCode::KindChanged,
        "set[0].behind",
    );
    let three = |set: Value| {
        json!({"id":"x","put":[{"view3d":{"id":"v"}},
            {"solid":{"in":"v","id":"s","shape":"box","size":[1,1,1]}}],"set":set})
    };
    for anchor in ["v", "s"] {
        refused(
            base,
            &three(json!([{"target":"c1","behind":anchor}])),
            ErrorCode::Unsupported,
            "set[0].behind",
        );
    }
    for target in ["v", "s"] {
        refused(
            base,
            &three(json!([{"target":target,"behind":"c1"}])),
            ErrorCode::KindChanged,
            "set[0].behind",
        );
    }
}

#[test]
fn a_behind_id_is_never_given_to_a_generated_mermaid_object() {
    refused(
        &[cloud()],
        &json!({"id":"graph","put":[
            {"diagram":{"id":"target"}},
            {"mermaid":{"in":"target","src":"flowchart TB\nA\n"}}
        ],"set":[{"target":"c1","behind":"--2.0"}]}),
        ErrorCode::UnknownRef,
        "set[0].behind",
    );
}

#[test]
fn a_stored_step_written_before_behind_existed_still_decodes() {
    // The step event inside a stored row, in the form the format before `behind` wrote.
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
