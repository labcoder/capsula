//! Canonical identity, validation, cancellation and recovery regressions.
use kumochi_scene::{
    Admission, Admitted, Begin, Budget, CallState, Committed, ErrorCode, GlobalBudget, Handle,
    Record, SceneEvent, SceneState, Step, StopCut, StopOutcome, WireId, parse_record,
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
fn admit(state: &SceneState, global: &GlobalBudget, step: &Step) -> Admitted {
    let Admission::New(admitted) = state.admit(step, global).expect("valid step") else {
        panic!("new step");
    };
    admitted
}
fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Committed {
    state
        .commit(admit(state, global, &step(json)), global)
        .expect("fresh admission")
}
fn cut(activated: u32, revealed: Vec<Handle>) -> StopCut {
    StopCut {
        activated,
        revealed: kumochi_scene::text::Bounded::new(revealed).expect("bounded handles"),
    }
}
fn text(name: &str, object: &str) -> String {
    format!(r#"{{"id":"{name}","put":[{{"text":{{"id":"{object}","text":"","at":[0,0]}}}}]}}"#)
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
        .expect_err("invalid admission");
    assert_eq!(error.code, code, "semantic error classification");
    assert_eq!(error.path.as_str(), path, "semantic error path");
    assert_eq!(
        before,
        (
            state.revision(),
            state.object_count(),
            global.generation(),
            state.recovery_log().len()
        ),
        "rejected admission leaves state and counters unchanged"
    );
}

#[test]
fn the_ingress_gate_refuses_3d_ops_and_2d_tracks_refuse_3d_keys() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    commit(&mut state, &mut global, &text("first", "t"));
    let before = (
        state.revision(),
        state.usage(),
        global.usage(),
        global.generation(),
        state.recovery_log().len(),
    );
    for op in [
        r#"{"view3d":{"id":"v"}}"#,
        r#"{"solid":{"id":"s","in":"missing","shape":"box"}}"#,
        r#"{"anim":{"target":"missing","track":"pos3","keys":[[0,[0,0,0]]]}}"#,
        r#"{"anim":{"target":"t","track":"pos3","keys":[[0,[0,0]]]}}"#,
        r#"{"anim":{"target":"t","track":"rot3","keys":[[0,[0,0,0]]]}}"#,
        r#"{"anim":{"target":"t","track":"rot3","keys":[[0,1]]}}"#,
    ] {
        let input = format!(
            r#"{{"id":"retry","put":[{{"text":{{"id":"new","text":"","at":[0,0]}}}},{op}]}}"#
        );
        let error = kumochi_scene::refuse_three_dimensional(&step(&input))
            .expect_err("3D waits for prepared views");
        assert_eq!(
            (error.code, error.path.as_str()),
            (ErrorCode::Unsupported, "put[1]")
        );
    }
    assert_eq!(
        kumochi_scene::refuse_three_dimensional(&step(&text("plain", "u"))),
        Ok(())
    );
    for track in ["x", "pos", "opacity"] {
        let input = format!(
            r#"{{"id":"retry","put":[{{"anim":{{"target":"t","track":"{track}","keys":[[0,[1,2,3]]]}}}}]}}"#
        );
        rejected(
            &state,
            &global,
            &input,
            ErrorCode::OutOfRange,
            "put[0].anim.keys[0][1]",
        );
        assert_eq!(
            (
                state.revision(),
                state.usage(),
                global.usage(),
                global.generation(),
                state.recovery_log().len()
            ),
            before
        );
    }
    let next = commit(&mut state, &mut global, &text("retry", "new"));
    assert_eq!(next.revision, 2);
    assert_eq!(state.object_count(), 2);
}

#[test]
fn same_step_references_and_expanded_slots_are_deterministic() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let committed = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[
        {"node":{"id":"a","in":"d","label":""}},
        {"anim":{"target":"a","track":"opacity","keys":[[0,0],[100,1]]}},
        {"diagram":{"id":"d"}},
        {"node":{"id":"b","in":"d","label":""}},
        {"edge":{"id":"e","in":"d","from":"a","to":"b"}}
    ]}"#,
    );
    assert_eq!((committed.index, committed.revision), (0, 1));
    assert_eq!(
        committed
            .handles
            .iter()
            .map(|(_, h)| h.get())
            .collect::<Vec<_>>(),
        [1024, 1026, 1027, 1028]
    );
    for _ in 0..2 {
        let replay = SceneState::replay(header(), state.recovery_log()).expect("valid recovery");
        for (id, handle) in &committed.handles {
            assert_eq!(replay.handle(id), Some(*handle));
        }
    }
}

#[test]
fn invalid_semantics_never_consume_ids_or_generations() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    rejected(
        &state,
        &global,
        r#"{"id":"bad","put":[{"text":{"id":"_1","at":{"ref":"later"},"text":""}}]}"#,
        ErrorCode::UnknownRef,
        "put[0].text.at.ref",
    );
    commit(&mut state, &mut global, &text("first", "_1"));
    rejected(
        &state,
        &global,
        r#"{"id":"later","start":{"after":"future","ms":0}}"#,
        ErrorCode::ForwardRef,
        "start.after",
    );
    rejected(
        &state,
        &global,
        r#"{"id":"cycle","put":[
        {"text":{"id":"a","at":{"ref":"b"},"text":""}},
        {"text":{"id":"b","at":{"ref":"a"},"text":""}}]}"#,
        ErrorCode::ReferenceCycle,
        "put[0]",
    );
    rejected(
        &state,
        &global,
        r#"{"id":"kind","set":[{"target":"_1","cells":[]}]}"#,
        ErrorCode::KindChanged,
        "set[0].cells",
    );
    rejected(
        &state,
        &global,
        r#"{"id":"property","put":[{"anim":{"target":"_1","track":"draw","keys":[[0,0],[10,1]]}}]}"#,
        ErrorCode::PropertyNotOwned,
        "put[0].anim.track",
    );
    assert_eq!(
        parse_record(br#"{"step":{"id":"anonymous","put":[{"text":{"at":[0,0],"text":""}}]}}"#)
            .expect_err("ids are required")
            .code,
        ErrorCode::MissingField
    );
    commit(
        &mut state,
        &mut global,
        r#"{"id":"remove","remove":["_1"]}"#,
    );
    state.retire(1, &mut global).expect("known step");
    rejected(
        &state,
        &global,
        &text("reuse", "_1"),
        ErrorCode::IdReused,
        "put[0].text.id",
    );
}

#[test]
fn duplicate_steps_compare_typed_values_even_after_being_dropped() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    commit(&mut state, &mut global, &text("first", "a"));
    let original = step(&text("second", "b"));
    state
        .commit(admit(&state, &global, &original), &mut global)
        .expect("fresh");
    state
        .stop(
            &cut(1, vec![Handle::new(1, 0).expect("valid handle")]),
            &mut global,
        )
        .expect("valid cut");
    let generation = global.generation();
    assert!(matches!(
        state.admit(&original, &global),
        Ok(Admission::Duplicate)
    ));
    assert_eq!(global.generation(), generation);
    rejected(
        &state,
        &global,
        &text("second", "different"),
        ErrorCode::DuplicateStepConflict,
        "id",
    );
    rejected(
        &state,
        &global,
        r#"{"id":"third","start":{"after":"second","ms":0}}"#,
        ErrorCode::ForwardRef,
        "start.after",
    );
}

#[test]
fn removals_check_orphans_and_stop_undoes_unactivated_removals() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let created = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[
        {"diagram":{"id":"d"}},{"node":{"id":"a","in":"d","label":""}},
        {"node":{"id":"b","in":"d","label":""}},{"edge":{"id":"e","in":"d","from":"a","to":"b"}}]}"#,
    );
    rejected(
        &state,
        &global,
        r#"{"id":"bad","remove":["a"]}"#,
        ErrorCode::Orphans,
        "remove[0]",
    );
    commit(
        &mut state,
        &mut global,
        r#"{"id":"second","remove":["a","e"]}"#,
    );
    assert_eq!(state.object_count(), 2);
    let handles: Vec<_> = created.handles.iter().map(|(_, h)| *h).collect();
    state
        .stop(&cut(1, handles), &mut global)
        .expect("undo unactivated removal");
    assert_eq!(
        (state.revision(), state.committed(), state.object_count()),
        (2, 1, 4)
    );
    let third = commit(&mut state, &mut global, &text("third", "c"));
    assert_eq!((third.index, third.handles[0].1.get()), (2, 3072));
    let replay = SceneState::replay(header(), state.recovery_log()).expect("replay");
    for (id, handle) in created.handles.iter().chain(&third.handles) {
        assert_eq!(replay.handle(id), Some(*handle));
    }
}

#[test]
fn ordered_cuts_preserve_stop_append_stop_dependencies() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let first = commit(
        &mut state,
        &mut global,
        r#"{"id":"first","put":[
        {"text":{"id":"a","text":"","at":[0,0]}},
        {"callout":{"id":"b","text":"","target":{"ref":"a"}}}]}"#,
    );
    let a = first.handles[0].1;
    state
        .stop(&cut(1, vec![a]), &mut global)
        .expect("drop unfinished dependant");
    let second = commit(
        &mut state,
        &mut global,
        r#"{"id":"second","put":[
        {"text":{"id":"c","text":"","at":[0,0]}}],"remove":["a"]}"#,
    );
    state
        .stop(&cut(2, vec![second.handles[0].1]), &mut global)
        .expect("keep all");
    assert_eq!(state.recovery_log().len(), 3);
    assert!(matches!(state.recovery_log()[1], SceneEvent::Stop(_)));
    let replay = SceneState::replay(header(), state.recovery_log()).expect("interleaved replay");
    assert_eq!(
        (replay.revision(), replay.committed(), replay.object_count()),
        (2, 2, 1)
    );
    assert_eq!(replay.handle(&id("c")), Some(second.handles[0].1));
    assert_eq!(replay.handle(&id("a")), None);
    assert_eq!(replay.handle(&id("b")), None);
    rejected(
        &state,
        &global,
        &text("third", "b"),
        ErrorCode::IdReused,
        "put[0].text.id",
    );
}

#[test]
fn stale_admissions_bind_scene_global_instance_and_global_generation() {
    let mut a = scene();
    let mut b = scene();
    let mut global = GlobalBudget::default();
    let candidate = admit(&a, &global, &step(&text("a", "a")));
    assert_eq!(
        b.commit(candidate, &mut global),
        Err(kumochi_scene::StaleAdmission)
    );
    let candidate = admit(&a, &global, &step(&text("a", "a")));
    assert_eq!(
        a.commit(candidate, &mut GlobalBudget::default()),
        Err(kumochi_scene::StaleAdmission)
    );
    let candidate = admit(&a, &global, &step(&text("a", "a")));
    commit(&mut b, &mut global, &text("b", "b"));
    let generation = global.generation();
    assert_eq!(
        a.commit(candidate, &mut global),
        Err(kumochi_scene::StaleAdmission)
    );
    assert_eq!(
        (a.revision(), a.object_count(), global.generation()),
        (0, 0, generation)
    );
    let candidate = admit(&a, &global, &step(&text("a", "a")));
    rejected(
        &a,
        &global,
        r#"{"id":"bad","remove":["missing"]}"#,
        ErrorCode::UnknownRef,
        "remove[0]",
    );
    a.commit(candidate, &mut global)
        .expect("rejected admission did not invalidate candidate");
    let candidate = admit(&a, &global, &step(&text("later", "later")));
    a.clear(&mut global).expect("matching budget context");
    assert_eq!(
        a.commit(candidate, &mut global),
        Err(kumochi_scene::StaleAdmission)
    );
}

#[test]
fn stop_zero_and_invalid_cuts_are_atomic_and_revisions_never_decrease() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let first = commit(&mut state, &mut global, &text("first", "a"));
    let generation = global.generation();
    for (bad, code, path) in [
        (cut(2, vec![]), ErrorCode::OutOfRange, "stop.activated"),
        (
            cut(1, vec![first.handles[0].1; 2]),
            ErrorCode::DuplicateKey,
            "stop.revealed[1]",
        ),
        (
            cut(0, vec![first.handles[0].1]),
            ErrorCode::UnknownRef,
            "stop.revealed[0]",
        ),
    ] {
        let error = state.stop(&bad, &mut global).expect_err("invalid cut");
        assert_eq!(error.code, code);
        assert_eq!(error.path.as_str(), path);
        assert_eq!(
            (state.revision(), state.object_count(), global.generation()),
            (1, 1, generation)
        );
    }
    assert_eq!(
        state
            .stop(&cut(0, vec![]), &mut global)
            .expect("clear before activation"),
        StopOutcome::Cleared
    );
    assert_eq!(
        (state.revision(), state.object_count(), state.committed()),
        (1, 0, 0)
    );
    let generation = global.generation();
    state.clear(&mut global).expect("matching budget context");
    assert_eq!(global.generation(), generation);
    rejected(
        &state,
        &global,
        &text("later", "b"),
        ErrorCode::SceneCleared,
        "",
    );
    let replay = SceneState::replay(header(), state.recovery_log()).expect("cleared recovery");
    assert!(replay.is_cleared());
    assert_eq!(replay.revision(), 1);
}

#[test]
fn lifetime_steps_include_dropped_steps_and_duplicate_is_still_a_noop_at_cap() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    for n in 1..=64 {
        let json = if n == 1 {
            text("s1", "discarded")
        } else {
            format!(r#"{{"id":"s{n}"}}"#)
        };
        commit(&mut state, &mut global, &json);
        state
            .stop(&cut(1, vec![]), &mut global)
            .expect("drop unfinished or later work");
    }
    assert_eq!(
        (
            state.revision(),
            state.committed(),
            state.recovery_log().len()
        ),
        (64, 1, 128)
    );
    rejected(
        &state,
        &global,
        r#"{"id":"too_many"}"#,
        ErrorCode::OverBudget(Budget::Steps),
        "",
    );
    assert!(matches!(
        state.admit(&step(r#"{"id":"s64"}"#), &global),
        Ok(Admission::Duplicate)
    ));
    let replay = SceneState::replay(header(), state.recovery_log()).expect("bounded recovery");
    assert_eq!((replay.revision(), replay.committed()), (64, 1));
}

#[test]
fn a_continuation_may_leave_out_size_and_restate_others() {
    let Record::Begin(stored) = record(
        r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"","size":[600,400]}}"#,
    ) else {
        panic!("begin fixture");
    };
    let continuation =
        record(r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"","others":"keep"}}"#);
    CallState::default()
        .accept(&continuation, &stored)
        .expect("the stored header keeps its canvas and others");
}

#[test]
fn call_guards_allow_typed_equal_continuations_but_not_records_after_end() {
    let header = header();
    let mut call = CallState::default();
    let item = Record::Step(step(r#"{"id":"first"}"#));
    assert_eq!(
        call.accept(&item, &header).expect_err("needs begin").code,
        ErrorCode::BeforeBegin
    );
    let different = record(
        r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"","size":[600,400]}}"#,
    );
    assert_eq!(
        call.accept(&different, &header)
            .expect_err("same scene needs the same canvas")
            .code,
        ErrorCode::HeaderMismatch
    );
    for (other, reason) in [
        (
            r#"{"begin":{"v":1,"scene":"scene","surface":"inline","alt":""}}"#,
            "same scene needs the same surface",
        ),
        (
            r#"{"begin":{"v":1,"scene":"other","surface":"overlay","alt":""}}"#,
            "a continuation names its own scene",
        ),
    ] {
        assert_eq!(
            call.accept(&record(other), &header).expect_err(reason).code,
            ErrorCode::HeaderMismatch,
            "{reason}"
        );
    }
    call.accept(&Record::Begin(header.clone()), &header)
        .expect("begin");
    assert_eq!(
        call.finish().expect_err("needs end").code,
        ErrorCode::MissingField
    );
    call.accept(&item, &header).expect("step");
    call.accept(&record(r#"{"end":{}}"#), &header).expect("end");
    call.finish().expect("complete");
    assert_eq!(
        call.accept(&item, &header).expect_err("after end").code,
        ErrorCode::AfterEnd
    );
    let mut described = header.clone();
    described.alt = "now with rain".to_owned().try_into().expect("bounded text");
    described.title = Some("Rain".to_owned().try_into().expect("bounded text"));
    let mut continuation = CallState::default();
    continuation
        .accept(&Record::Begin(described), &header)
        .expect("a new call may restate the title and alt");
    continuation
        .accept(&item, &header)
        .expect("scene can continue");
}

#[test]
fn retirement_is_idempotent_and_handle_codec_rejects_unused_bits() {
    let mut state = scene();
    let mut global = GlobalBudget::default();
    let first = commit(&mut state, &mut global, &text("first", "a"));
    state.retire(0, &mut global).expect("known step");
    let generation = global.generation();
    state
        .retire(0, &mut global)
        .expect("duplicate notification");
    state
        .stop(&cut(1, vec![first.handles[0].1]), &mut global)
        .expect("keep everything");
    assert_eq!(global.generation(), generation);
    assert_eq!(state.recovery_log().len(), 1);
    for raw in [0, 1023, 1024 + 856, 65 << 10] {
        assert_eq!(Handle::try_from(raw), Err(ErrorCode::OutOfRange));
    }
    let high = Handle::new(64, 855).expect("maximum expanded slot");
    assert_eq!((high.revision(), high.slot(), high.get()), (64, 855, 66391));
    let encoded =
        postcard::to_allocvec(&SceneEvent::Stop(cut(1, vec![first.handles[0].1]))).expect("encode");
    assert_eq!(encoded, [1, 1, 1, 128, 8]);
    assert_eq!(
        postcard::from_bytes::<SceneEvent>(&encoded).expect("decode"),
        SceneEvent::Stop(cut(1, vec![first.handles[0].1]))
    );
}
#[test]
fn semantic_field_errors_are_precise_and_content_free() {
    let state = scene();
    let global = GlobalBudget::default();
    for (json, code, path) in [
        (
            r#"{"id":"bad","put":[{"text":{"id":"a","text":"","at":[0,0]}},{"text":{"id":"a","text":"","at":[0,0]}}]}"#,
            ErrorCode::IdReused,
            "put[1].text.id",
        ),
        (
            r#"{"id":"bad","put":[{"row":{"id":"n","cells":[]}}]}"#,
            ErrorCode::MissingField,
            "put[0].row.in",
        ),
        (
            r#"{"id":"bad","put":[{"table":{"id":"t","cols":[]}},{"node":{"id":"n","label":"","in":"t"}}]}"#,
            ErrorCode::OutOfRange,
            "put[1].node.in",
        ),
        (
            r#"{"id":"bad","put":[{"text":{"id":"a","text":"","at":[0,0],"opacity":2}}]}"#,
            ErrorCode::OutOfRange,
            "put[0].text.opacity",
        ),
        (
            r#"{"id":"bad","put":[{"text":{"id":"a","text":"","at":[0,0],"size":0}}]}"#,
            ErrorCode::OutOfRange,
            "put[0].text.size",
        ),
        (
            r#"{"id":"bad","put":[{"text":{"id":"a","text":"","at":[0,0]}},{"anim":{"target":"a","track":"opacity","keys":[[10,0],[10,1]]}}]}"#,
            ErrorCode::OutOfRange,
            "put[1].anim.keys[1][0]",
        ),
        (
            r#"{"id":"bad","put":[{"text":{"id":"a","text":"","at":[0,0]}},{"anim":{"target":"a","track":"pos","keys":[[0,1]]}}]}"#,
            ErrorCode::OutOfRange,
            "put[1].anim.keys[0][1]",
        ),
    ] {
        rejected(&state, &global, json, code, path);
    }
    let Record::Begin(bad) =
        record(r#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":"","size":[100,0]}}"#)
    else {
        panic!("begin fixture");
    };
    let error = SceneState::begin(bad).expect_err("positive dimensions");
    assert_eq!(error.code, ErrorCode::OutOfRange);
    assert_eq!(error.path.as_str(), "size[1]");
}

#[test]
fn recovery_rejects_nonoriginal_revisions_duplicate_steps_and_redundant_cuts() {
    let first = step(r#"{"id":"first"}"#);
    for log in [
        vec![SceneEvent::Step {
            revision: 2,
            step: first.clone().into(),
        }],
        vec![
            SceneEvent::Step {
                revision: 1,
                step: first.clone().into(),
            },
            SceneEvent::Step {
                revision: 1,
                step: first.clone().into(),
            },
        ],
    ] {
        assert_eq!(
            SceneState::replay(header(), &log)
                .expect_err("revision must be original")
                .code,
            ErrorCode::OutOfRange
        );
    }
    assert_eq!(
        SceneState::replay(
            header(),
            &[
                SceneEvent::Step {
                    revision: 1,
                    step: first.clone().into()
                },
                SceneEvent::Step {
                    revision: 2,
                    step: first.clone().into()
                },
            ]
        )
        .expect_err("a duplicate is not another commit")
        .code,
        ErrorCode::DuplicateStepConflict
    );
    assert_eq!(
        SceneState::replay(
            header(),
            &[
                SceneEvent::Step {
                    revision: 1,
                    step: first.into()
                },
                SceneEvent::Stop(cut(1, vec![])),
            ]
        )
        .expect_err("keep-all cuts are not log entries")
        .code,
        ErrorCode::Malformed
    );
}
