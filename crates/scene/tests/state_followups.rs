//! Additional semantic-boundary and Stop/replay conformance cases.
use kumochi_scene::{
    Admission, Begin, CallState, ErrorCode, GlobalBudget, Handle, Record, SceneEvent, SceneState,
    Step, StopCut, StopOutcome, WireId, parse_record, text::Bounded,
};

fn record(json: &str) -> Record {
    parse_record(json.as_bytes()).expect("valid fixture")
}
fn header() -> Begin {
    let Record::Begin(header) =
        record(r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#)
    else {
        panic!("begin fixture");
    };
    header
}
fn scene() -> SceneState {
    SceneState::begin(header()).expect("valid header")
}
fn step(json: &str) -> Step {
    let Record::Step(step) = record(&format!(r#"{{"step":{json}}}"#)) else {
        panic!("step fixture");
    };
    step
}
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("valid id")
}
fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Vec<Handle> {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state
        .commit(admitted, global)
        .expect("fresh candidate")
        .handles
        .into_iter()
        .map(|(_, h)| h)
        .collect()
}
fn cut(activated: u32, handles: Vec<Handle>) -> StopCut {
    StopCut {
        activated,
        revealed: Bounded::new(handles).expect("bounded handles"),
    }
}
fn rejected(state: &SceneState, global: &GlobalBudget, json: &str, code: ErrorCode, path: &str) {
    let before = (
        state.revision(),
        state.object_count(),
        global.generation(),
        state.recovery_log().len(),
    );
    let error = state
        .admit(&step(json), global)
        .expect_err("invalid semantic fixture");
    assert_eq!(error.code, code, "exact semantic code");
    assert_eq!(error.path.as_str(), path, "exact content-free field path");
    assert_eq!(
        before,
        (
            state.revision(),
            state.object_count(),
            global.generation(),
            state.recovery_log().len()
        ),
        "rejection changes nothing"
    );
}

#[test]
fn every_positive_dimension_and_box_component_rejects_zero() {
    let state = scene();
    let global = GlobalBudget::default();
    for (op, path) in [
        (
            r#"{"line":{"id":"t","from":[0,0],"to":[1,1],"width":0}}"#,
            "line.width",
        ),
        (
            r#"{"arrow":{"id":"t","from":[0,0],"to":[1,1],"width":0}}"#,
            "arrow.width",
        ),
        (r#"{"path":{"id":"t","d":"M0 0","width":0}}"#, "path.width"),
        (r#"{"path":{"id":"t","d":"M0 0","scale":0}}"#, "path.scale"),
        (
            r#"{"shape":{"id":"t","kind":"rect","width":0}}"#,
            "shape.width",
        ),
        (r#"{"shape":{"id":"t","kind":"rect","r":0}}"#, "shape.r"),
        (
            r#"{"shape":{"id":"t","kind":"rect","radius":0}}"#,
            "shape.radius",
        ),
        (
            r#"{"shape":{"id":"t","kind":"rect","size":[0,1]}}"#,
            "shape.size[0]",
        ),
        (
            r#"{"shape":{"id":"t","kind":"rect","size":[1,0]}}"#,
            "shape.size[1]",
        ),
        (
            r#"{"shape":{"id":"t","kind":"rect","box":[0,0,0,1]}}"#,
            "shape.box[2]",
        ),
        (
            r#"{"shape":{"id":"t","kind":"rect","box":[0,0,1,0]}}"#,
            "shape.box[3]",
        ),
        (
            r#"{"text":{"id":"t","at":[0,0],"text":"","size":0}}"#,
            "text.size",
        ),
        (
            r#"{"text":{"id":"t","at":[0,0],"text":"","max_width":0}}"#,
            "text.max_width",
        ),
        (
            r#"{"callout":{"id":"t","target":[0,0],"text":"","max_width":0}}"#,
            "callout.max_width",
        ),
        (
            r#"{"highlight":{"id":"t","target":[0,0],"pad":0}}"#,
            "highlight.pad",
        ),
        (
            r#"{"diagram":{"id":"t","box":[0,0,0,1]}}"#,
            "diagram.box[2]",
        ),
        (
            r#"{"diagram":{"id":"t","box":[0,0,1,0]}}"#,
            "diagram.box[3]",
        ),
        (
            r#"{"table":{"id":"t","cols":[],"box":[0,0,0,1]}}"#,
            "table.box[2]",
        ),
        (
            r#"{"table":{"id":"t","cols":[],"box":[0,0,1,0]}}"#,
            "table.box[3]",
        ),
        (r#"{"chart":{"id":"t","box":[0,0,0,1]}}"#, "chart.box[2]"),
        (r#"{"chart":{"id":"t","box":[0,0,1,0]}}"#, "chart.box[3]"),
    ] {
        rejected(
            &state,
            &global,
            &format!(r#"{{"id":"bad","put":[{op}]}}"#),
            ErrorCode::OutOfRange,
            &format!("put[0].{path}"),
        );
    }
}

#[test]
fn motion_kinds_ranges_and_combined_time_limits_are_exact() {
    let state = scene();
    let global = GlobalBudget::default();
    for (anim, path) in [
        (
            r#"{"target":"t","track":"opacity","keys":[[0,-1]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"opacity","keys":[[0,2]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"draw","keys":[[0,-1]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"draw","keys":[[0,2]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"scale","keys":[[0,0]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"x","keys":[[0,"red"]]}"#,
            "keys[0][1]",
        ),
        (
            r#"{"target":"t","track":"color","keys":[[0,1]]}"#,
            "keys[0][1]",
        ),
        (r#"{"target":"t","track":"x","keys":[]}"#, "keys"),
        (
            r#"{"target":"t","track":"x","keys":[[0,1]],"repeat":-2}"#,
            "repeat",
        ),
        (
            r#"{"target":"t","track":"x","keys":[[1,1]],"delay":60000}"#,
            "keys[0][0]",
        ),
    ] {
        rejected(
            &state,
            &global,
            &format!(
                r#"{{"id":"bad","put":[{{"line":{{"id":"t","from":[0,0],"to":[1,1]}}}},{{"anim":{anim}}}]}}"#
            ),
            ErrorCode::OutOfRange,
            &format!("put[1].anim.{path}"),
        );
    }
    rejected(
        &state,
        &global,
        r#"{"id":"bad","put":[{"text":{"id":"t","at":[0,0],"text":"","delay":60000,"dur":1}}]}"#,
        ErrorCode::OutOfRange,
        "put[0].text.dur",
    );

    let mut state = scene();
    let mut global = GlobalBudget::default();
    commit(
        &mut state,
        &mut global,
        r#"{"id":"boundary","put":[
        {"line":{"id":"t","from":[0,0],"to":[1,1],"opacity":0,"width":1,"delay":0,"dur":60000}},
        {"anim":{"target":"t","track":"opacity","keys":[[0,0],[60000,1]],"repeat":-1}},
        {"anim":{"target":"t","track":"draw","keys":[[0,0],[60000,1]]}},
        {"anim":{"target":"t","track":"scale","keys":[[0,1]]}},
        {"anim":{"target":"t","track":"color","keys":[[0,"red"]]}}
    ]}"#,
    );
    assert_eq!((state.revision(), state.object_count()), (1, 1));
}

#[test]
fn missing_targets_endpoint_kinds_and_duplicate_removals_are_rejected() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[{"text":{"id":"a","at":[0,0],"text":""}}]}"#,
    );
    for (json, code, path) in [
        (
            r#"{"id":"bad","set":[{"target":"missing","text":""}]}"#,
            ErrorCode::UnknownRef,
            "set[0].target",
        ),
        (
            r#"{"id":"bad","put":[{"anim":{"target":"missing","track":"x","keys":[[0,1]]}}]}"#,
            ErrorCode::UnknownRef,
            "put[0].anim.target",
        ),
        (
            r#"{"id":"bad","remove":["a","a"]}"#,
            ErrorCode::DuplicateKey,
            "remove[1]",
        ),
        (
            r#"{"id":"bad","put":[{"mermaid":{"in":"d","src":""}}]}"#,
            ErrorCode::Malformed,
            "put[0].mermaid.src",
        ),
        (
            r#"{"id":"bad","put":[{"diagram":{"id":"d"}},{"edge":{"id":"e","in":"d","from":"d","to":"d"}}]}"#,
            ErrorCode::OutOfRange,
            "put[1].edge.from",
        ),
        (
            r#"{"id":"bad","put":[{"diagram":{"id":"d"}},{"diagram":{"id":"other"}},
            {"node":{"id":"n","in":"d","label":""}},{"node":{"id":"m","in":"other","label":""}},
            {"edge":{"id":"e","in":"d","from":"n","to":"m"}}]}"#,
            ErrorCode::OutOfRange,
            "put[4].edge.to",
        ),
    ] {
        rejected(&state, &global, json, code, path);
    }
}

#[test]
fn preparation_failure_is_an_ordered_cut_before_the_next_call() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let x = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[{"text":{"id":"x","at":[0,0],"text":""}}]}"#,
    )[0];
    state.retire(0, &mut global).expect("first step shown");
    commit(&mut state, &mut global, r#"{"id":"failed","remove":["x"]}"#);
    assert_eq!(state.handle(&id("x")), None);
    assert_eq!(
        state
            .stop(&cut(1, vec![x]), &mut global)
            .expect("failure cut"),
        StopOutcome::Kept
    );
    assert_eq!((state.usage().objects, state.usage().scenes), (1, 1));
    assert_eq!(global.usage(), state.usage());
    let before = (state.usage(), global.generation());
    state
        .stop(&cut(1, vec![x]), &mut global)
        .expect("repeat cut is inert");
    assert_eq!((state.usage(), global.generation()), before);
    let restored = SceneState::replay(header(), state.recovery_log()).expect("cut usage replay");
    assert_eq!(restored.usage(), state.usage());
    let next = commit(
        &mut state,
        &mut global,
        r#"{"id":"next_call","put":[{"callout":{"id":"c","target":{"ref":"x"},"text":""}}]}"#,
    )[0];
    assert_eq!(
        (state.revision(), state.committed(), state.object_count()),
        (3, 2, 2)
    );
    assert_eq!(next.get(), 3072);
    assert!(state.step(1).is_none());
    assert!(state.step(2).is_some_and(|step| step.id == id("next_call")));
    let replay = SceneState::replay(header(), state.recovery_log()).expect("failure replay");
    assert_eq!(replay.handle(&id("x")), Some(x));
    assert_eq!(replay.handle(&id("c")), Some(next));
    assert_eq!(replay.recovery_log(), state.recovery_log());
    assert_eq!((state.usage().objects, state.usage().scenes), (2, 1));
    assert_eq!(global.usage(), state.usage());
    assert_eq!(replay.usage(), state.usage());
}

#[test]
fn first_step_preparation_failure_clears_without_an_extra_event_kind() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    commit(
        &mut state,
        &mut global,
        r#"{"id":"failed","put":[{"text":{"id":"x","at":[0,0],"text":""}}]}"#,
    );
    assert_eq!(
        state
            .stop(&cut(0, vec![]), &mut global)
            .expect("first-step failure"),
        StopOutcome::Cleared
    );
    assert_eq!(
        (
            state.revision(),
            state.committed(),
            state.object_count(),
            state.recovery_log().len()
        ),
        (1, 0, 0, 2)
    );
    assert!(matches!(
        &state.recovery_log()[1],
        SceneEvent::Stop(StopCut { activated: 0, .. })
    ));
    let replay =
        SceneState::replay(header(), state.recovery_log()).expect("cleared failure replay");
    assert!(replay.is_cleared());
    assert_eq!((replay.revision(), replay.committed()), (1, 0));
}

#[test]
fn an_effective_cut_after_a_gap_preserves_original_handles_and_reservations() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let a = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[{"text":{"id":"a","at":[0,0],"text":""}}]}"#,
    )[0];
    commit(
        &mut state,
        &mut global,
        r#"{"id":"second","put":[{"text":{"id":"discarded","at":[0,0],"text":""}}]}"#,
    );
    state
        .stop(&cut(1, vec![a]), &mut global)
        .expect("drop revision two");
    rejected(
        &state,
        &global,
        r#"{"id":"reuse","put":[{"text":{"id":"discarded","at":[0,0],"text":""}}]}"#,
        ErrorCode::IdReused,
        "put[0].text.id",
    );
    let c = commit(
        &mut state,
        &mut global,
        r#"{"id":"third","put":[{"text":{"id":"c","at":[0,0],"text":""}}]}"#,
    )[0];
    commit(
        &mut state,
        &mut global,
        r#"{"id":"fourth","put":[{"text":{"id":"d","at":[0,0],"text":""}}]}"#,
    );
    let error = state
        .stop(&cut(2, vec![a]), &mut global)
        .expect_err("dropped revision is not activated");
    assert_eq!(error.code, ErrorCode::OutOfRange);
    assert_eq!(error.path.as_str(), "stop.activated");
    state
        .stop(&cut(3, vec![a, c]), &mut global)
        .expect("revision bound, not kept count");
    assert_eq!(
        (state.revision(), state.committed(), state.object_count()),
        (4, 2, 2)
    );
    assert_eq!(c.get(), 3072);
    for invalid in [cut(1, vec![a]), cut(3, vec![a])] {
        let generation = global.generation();
        let error = state
            .stop(&invalid, &mut global)
            .expect_err("backward or second effective cut");
        assert_eq!(error.code, ErrorCode::OutOfRange);
        assert_eq!(error.path.as_str(), "stop.activated");
        assert_eq!((global.generation(), state.object_count()), (generation, 2));
    }
    let replay = SceneState::replay(header(), state.recovery_log()).expect("gap replay");
    assert_eq!(replay.handle(&id("a")), Some(a));
    assert_eq!(replay.handle(&id("c")), Some(c));
    assert_eq!(replay.handle(&id("d")), None);
    assert_eq!((replay.revision(), replay.committed()), (4, 2));
}

#[test]
fn a_detached_finished_survivor_can_be_referenced_after_its_anchor_is_dropped() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let handles = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[
        {"text":{"id":"anchor","at":[0,0],"text":""}},
        {"callout":{"id":"survivor","target":{"ref":"anchor"},"text":""}}]}"#,
    );
    state
        .stop(&cut(1, vec![handles[1]]), &mut global)
        .expect("retain already placed dependant");
    assert_eq!(state.handle(&id("anchor")), None);
    let next = commit(
        &mut state,
        &mut global,
        r#"{"id":"second","put":[
        {"text":{"id":"next","at":{"ref":"survivor"},"text":""}}]}"#,
    )[0];
    let replay =
        SceneState::replay(header(), state.recovery_log()).expect("semantic detach replay");
    assert_eq!(replay.handle(&id("survivor")), Some(handles[1]));
    assert_eq!(replay.handle(&id("next")), Some(next));
}

#[test]
fn call_phase_and_oversized_replay_fail_at_the_exact_boundary() {
    let header = header();
    let mut call = CallState::default();
    assert_eq!(
        call.finish().expect_err("no begin").code,
        ErrorCode::BeforeBegin
    );
    call.accept(&Record::Begin(header.clone()), &header)
        .expect("first begin");
    let error = call
        .accept(&Record::Begin(header.clone()), &header)
        .expect_err("second begin");
    assert_eq!(error.code, ErrorCode::Malformed);
    assert_eq!(error.path.as_str(), "");
    call.accept(&record(r#"{"end":{}}"#), &header)
        .expect("rejected record did not change phase");
    call.finish().expect("complete");
    let event = SceneEvent::Step {
        revision: 1,
        step: step(r#"{"id":"first"}"#).into(),
    };
    assert_eq!(
        SceneState::replay(header, &vec![event; 129])
            .expect_err("one beyond log cap")
            .code,
        ErrorCode::TooLong
    );
}

#[test]
fn new_state_types_never_debug_print_ids_or_drawing_text() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let source = step(
        r#"{"id":"PRIVATE_STEP","put":[{"text":{"id":"PRIVATE_OBJECT","at":[0,0],"text":"PRIVATE_CONTENT"}}]}"#,
    );
    let Admission::New(candidate) = state.admit(&source, &global).expect("valid") else {
        panic!("new");
    };
    let candidate_debug = format!("{candidate:?}");
    let admission_debug = format!("{:?}", Admission::New(candidate));
    let Admission::New(candidate) = state.admit(&source, &global).expect("pure") else {
        panic!("new");
    };
    let committed = state.commit(candidate, &mut global).expect("fresh");
    let values = [
        candidate_debug,
        admission_debug,
        format!("{state:?}"),
        format!("{committed:?}"),
        format!(
            "{:?}",
            SceneEvent::Step {
                revision: 1,
                step: source.into()
            }
        ),
        format!("{:?}", cut(1, vec![committed.handles[0].1])),
    ];
    for value in values {
        for sentinel in ["PRIVATE_STEP", "PRIVATE_OBJECT", "PRIVATE_CONTENT"] {
            assert!(
                !value.contains(sentinel),
                "new Debug output is content-free"
            );
        }
    }
    assert_eq!(state.header(), &header());
    assert!(
        state
            .step(0)
            .is_some_and(|step| step.id == id("PRIVATE_STEP"))
    );
    assert!(state.step(u32::MAX).is_none());
}

#[test]
#[ignore = "normal-priority timing on a quiet Rez with the measurement lock"]
fn measure_whole_scene_removal_admit() {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };
    let mut state = scene();
    let mut global = GlobalBudget::default();
    for (name, range) in [("first", 0..300), ("second", 300..600)] {
        let operations = range
            .map(|n| format!(r#"{{"text":{{"id":"t{n}","at":[0,0],"text":""}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        commit(
            &mut state,
            &mut global,
            &format!(r#"{{"id":"{name}","put":[{operations}]}}"#),
        );
    }
    let ids = (0..600)
        .map(|n| format!(r#""t{n}""#))
        .collect::<Vec<_>>()
        .join(",");
    let removal = step(&format!(r#"{{"id":"remove","remove":[{ids}]}}"#));
    assert_eq!(state.object_count(), 600);
    for _ in 0..5 {
        black_box(state.admit(&removal, &global).expect("valid full removal"));
    }
    let mut total = Duration::ZERO;
    let mut longest = Duration::ZERO;
    for _ in 0..128 {
        let started = Instant::now();
        let candidate = state.admit(&removal, &global).expect("valid full removal");
        let elapsed = started.elapsed();
        total += elapsed;
        longest = longest.max(elapsed);
        black_box(candidate);
    }
    assert_eq!((state.revision(), state.object_count()), (2, 600));
    eprintln!(
        "admit: objects=600 removals=600 warmups=5 samples=128 mean_us={:.3} max_us={:.3}",
        total.as_secs_f64() * 1_000_000.0 / 128.0,
        longest.as_secs_f64() * 1_000_000.0
    );
}
