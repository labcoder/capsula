//! A clicked part's current text: its put, later kept sets, cuts, replay and the refusals.
use kumochi_scene::{
    Admission, Committed, GlobalBudget, Handle, Record, SceneState, Step, StopCut, parse_record,
    text::Bounded,
};

fn header() -> kumochi_scene::Begin {
    let source = br#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"a car"}}"#;
    let Ok(Record::Begin(header)) = parse_record(source) else {
        panic!("begin fixture");
    };
    header
}
fn step(json: &str) -> Step {
    let record = format!(r#"{{"step":{json}}}"#);
    match parse_record(record.as_bytes()) {
        Ok(Record::Step(step)) => step,
        other => panic!("step fixture: {other:?}"),
    }
}
fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Committed {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state.commit(admitted, global).expect("fresh admission")
}
fn handle(committed: &Committed, name: &str) -> Handle {
    committed
        .handles
        .iter()
        .find(|(id, _)| id.as_str() == name)
        .map(|&(_, handle)| handle)
        .expect("put in this step")
}
/// A view and a box solid "a" with `extra` fields.
fn first(state: &mut SceneState, global: &mut GlobalBudget, extra: &str) -> Committed {
    commit(
        state,
        global,
        &format!(
            r#"{{"id":"s1","put":[{{"view3d":{{"id":"v"}}}},{{"solid":{{"in":"v","id":"a","shape":"box","size":[2,2,2]{extra}}}}}]}}"#
        ),
    )
}
fn text(part: Option<kumochi_scene::Part<'_>>) -> Option<(String, Option<String>)> {
    part.map(|part| {
        (
            part.label.as_str().to_owned(),
            part.info.map(|info| info.as_str().to_owned()),
        )
    })
}
fn pair(label: &str, info: Option<&str>) -> (String, Option<String>) {
    (label.to_owned(), info.map(str::to_owned))
}

#[test]
fn later_kept_sets_apply_in_order_and_a_partial_set_keeps_the_other_field() {
    let (mut state, mut global) = (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    );
    let put = first(
        &mut state,
        &mut global,
        r#","label":"Wheel","info":"Front left""#,
    );
    let a = handle(&put, "a");
    assert_eq!(text(state.part(a)), Some(pair("Wheel", Some("Front left"))));
    commit(
        &mut state,
        &mut global,
        r#"{"id":"s2","set":[{"target":"a","label":"Hub"}]}"#,
    );
    assert_eq!(text(state.part(a)), Some(pair("Hub", Some("Front left"))));
    commit(
        &mut state,
        &mut global,
        r##"{"id":"s3","set":[{"target":"a","color":"#ff0000"}]}"##,
    );
    commit(
        &mut state,
        &mut global,
        r#"{"id":"s4","set":[{"target":"a","info":"Spins"}]}"#,
    );
    assert_eq!(text(state.part(a)), Some(pair("Hub", Some("Spins"))));
    commit(
        &mut state,
        &mut global,
        r#"{"id":"s5","set":[{"target":"a","label":"Rim","info":"Alloy"}]}"#,
    );
    assert_eq!(text(state.part(a)), Some(pair("Rim", Some("Alloy"))));
}

#[test]
fn a_set_a_cut_dropped_never_counts_even_after_replay() {
    let (mut state, mut global) = (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    );
    let put = first(&mut state, &mut global, r#","label":"Wheel""#);
    let a = handle(&put, "a");
    commit(
        &mut state,
        &mut global,
        r#"{"id":"s2","set":[{"target":"a","label":"Hub","info":"Late"}]}"#,
    );
    assert_eq!(text(state.part(a)), Some(pair("Hub", Some("Late"))));
    let revealed =
        Bounded::new(put.handles.iter().map(|&(_, handle)| handle).collect()).expect("two");
    state
        .stop(
            &StopCut {
                activated: 1,
                revealed,
            },
            &mut global,
        )
        .expect("valid cut");
    assert_eq!(text(state.part(a)), Some(pair("Wheel", None)));
    let replayed = SceneState::replay(header(), state.recovery_log()).expect("replays");
    assert_eq!(text(replayed.part(a)), Some(pair("Wheel", None)));
}

#[test]
fn a_solid_labelled_only_by_a_later_set_resolves_unless_that_set_was_cut() {
    let (mut state, mut global) = (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    );
    let put = first(&mut state, &mut global, "");
    let a = handle(&put, "a");
    assert_eq!(text(state.part(a)), None, "no label yet");
    commit(
        &mut state,
        &mut global,
        r#"{"id":"s2","set":[{"target":"a","label":"Late"}]}"#,
    );
    assert_eq!(text(state.part(a)), Some(pair("Late", None)));
    let revealed =
        Bounded::new(put.handles.iter().map(|&(_, handle)| handle).collect()).expect("two");
    state
        .stop(
            &StopCut {
                activated: 1,
                revealed,
            },
            &mut global,
        )
        .expect("valid cut");
    assert_eq!(
        text(state.part(a)),
        None,
        "the label lived only in the cut step"
    );
}

#[test]
fn removed_unknown_view_and_cleared_handles_give_nothing() {
    let (mut state, mut global) = (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    );
    let put = commit(
        &mut state,
        &mut global,
        r#"{"id":"s1","put":[{"view3d":{"id":"v"}},{"solid":{"in":"v","id":"a","shape":"box","size":[2,2,2],"label":"A"}},{"solid":{"in":"v","id":"b","shape":"box","size":[1,1,1],"label":"B"}}]}"#,
    );
    let (a, b) = (handle(&put, "a"), handle(&put, "b"));
    assert_eq!(
        text(state.part(handle(&put, "v"))),
        None,
        "a view is not a part"
    );
    assert_eq!(text(state.part(Handle::new(9, 0).expect("valid"))), None);
    commit(&mut state, &mut global, r#"{"id":"s2","remove":["b"]}"#);
    assert_eq!(text(state.part(b)), None, "removed");
    assert_eq!(text(state.part(a)), Some(pair("A", None)));
    state.clear(&mut global).expect("clears");
    assert_eq!(text(state.part(a)), None, "cleared");
}

#[test]
fn live_solids_sharing_a_slot_across_revisions_resolve_by_their_own_handles() {
    let (mut state, mut global) = (
        SceneState::begin(header()).expect("scene"),
        GlobalBudget::default(),
    );
    let older = first(&mut state, &mut global, r#","label":"Older""#);
    let newer = commit(
        &mut state,
        &mut global,
        r#"{"id":"s2","put":[{"solid":{"in":"v","id":"x","shape":"box","size":[1,1,1]}},{"solid":{"in":"v","id":"b","shape":"box","size":[1,1,1],"label":"Newer"}}]}"#,
    );
    let (a, b) = (handle(&older, "a"), handle(&newer, "b"));
    assert_eq!(
        (a.revision(), b.revision(), a.slot(), b.slot()),
        (1, 2, 1, 1)
    );
    assert_eq!(text(state.part(a)), Some(pair("Older", None)));
    assert_eq!(text(state.part(b)), Some(pair("Newer", None)));
}

#[test]
fn a_parts_debug_prints_lengths_never_its_text() {
    let debug = |fields| {
        let (mut state, mut global) = (
            SceneState::begin(header()).expect("scene"),
            GlobalBudget::default(),
        );
        let put = first(&mut state, &mut global, fields);
        format!("{:?}", state.part(handle(&put, "a")).expect("labelled"))
    };
    let shown = debug(r#","label":"SENTINELLABEL","info":"SENTINELINFO""#);
    assert!(
        shown == debug(r#","label":"ALTERNATEWORD","info":"ANOTHERWORDS""#),
        "part text affected Debug"
    );
    for sentinel in ["SENTINELLABEL", "SENTINELINFO"] {
        assert!(!shown.contains(sentinel), "part Debug contains text");
        let bytes = format!("{:?}", sentinel.as_bytes());
        let bytes = bytes.trim_matches(['[', ']']);
        assert!(!shown.contains(bytes), "part Debug contains text bytes");
    }
}
