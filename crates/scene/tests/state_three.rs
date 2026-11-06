//! 3D admission: views, solids, attachments, budgets and transform bounds.
use kumochi_scene::{
    Admission, BindingError, Budget, Committed, ErrorCode, GlobalBudget, Handle, Record,
    SceneBinding, SceneEvent, SceneState, Step, StopCut, StopOutcome, WireId,
    deferred::Deferred,
    parse_record,
    record::Op,
    text::Bounded,
    three::{
        Transform,
        mesh::{MeshScratch, build},
        size::{reach, triangles},
    },
};
use std::sync::atomic::AtomicBool;

#[path = "support/allocation.rs"]
mod allocation;

fn header() -> kumochi_scene::Begin {
    let source = br#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#;
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
fn put(name: &str, ops: &[String]) -> String {
    format!(r#"{{"id":"{name}","put":[{}]}}"#, ops.join(","))
}
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("valid id")
}
fn view(name: &str) -> String {
    format!(r#"{{"view3d":{{"id":"{name}"}}}}"#)
}
/// A 12-triangle box in view v.
fn cube(name: &str, extra: &str) -> String {
    format!(r#"{{"solid":{{"in":"v","id":"{name}","shape":"box","size":[2,2,2]{extra}}}}}"#)
}
fn sphere(name: &str, extra: &str) -> String {
    format!(r#"{{"solid":{{"in":"v","id":"{name}","shape":"sphere","r":1{extra}}}}}"#)
}
/// A lathe of radius 1 with 2 × segments × (points − 1) triangles.
fn lathe(name: &str, points: usize, segments: u8) -> String {
    let profile: Vec<String> = (0..points).map(|i| format!("[1,{i}]")).collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"lathe","points":[{}],"segments":{segments}}}}}"#,
        profile.join(",")
    )
}
fn track(target: &str, track: &str, to: [f32; 3], ease: &str) -> String {
    format!(
        r#"{{"anim":{{"target":"{target}","track":"{track}","keys":[[0,[0,0,0]],[1000,[{},{},{}]]],"ease":"{ease}"}}}}"#,
        to[0], to[1], to[2]
    )
}
fn scene() -> SceneState {
    SceneState::begin(header()).expect("valid header")
}
fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Committed {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state.commit(admitted, global).expect("fresh admission")
}
fn refused(state: &SceneState, global: &GlobalBudget, json: &str) -> (ErrorCode, String) {
    let before = (
        state.revision(),
        state.usage(),
        global.usage(),
        global.generation(),
    );
    let error = state
        .admit_checked(&step(json), global)
        .expect_err("refused step");
    assert_eq!(
        before,
        (
            state.revision(),
            state.usage(),
            global.usage(),
            global.generation()
        ),
        "a refusal moves no counter"
    );
    (error.code, error.path.as_str().to_owned())
}
fn over(budget: Budget) -> (ErrorCode, String) {
    (ErrorCode::OverBudget(budget), String::new())
}
fn at(code: ErrorCode, path: &str) -> (ErrorCode, String) {
    (code, path.to_owned())
}

#[test]
fn solids_per_scene_stop_at_160() {
    let mut boxes: Vec<String> = (0..160).map(|i| cube(&format!("s{i}"), "")).collect();
    boxes.insert(0, view("v"));
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    let all = put("all", &boxes);
    commit(&mut state, &mut global, &all);
    assert_eq!(
        (
            state.usage().solids,
            state.usage().triangles,
            state.usage().views
        ),
        (160, 1_920, 1)
    );
    let (fresh, global) = (scene(), GlobalBudget::default());
    boxes.push(cube("s160", ""));
    assert_eq!(
        refused(&fresh, &global, &put("over", &boxes)),
        over(Budget::Solids)
    );
}

#[test]
fn triangles_per_scene_stop_at_24000_and_refusals_leave_counters_alone() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put("a", &[view("v"), lathe("l", 251, 48)]),
    );
    assert_eq!(state.usage().triangles, 24_000);
    assert_eq!(
        refused(&state, &global, &put("b", &[cube("c", "")])),
        over(Budget::Triangles)
    );

    let (mut state, mut global) = (scene(), GlobalBudget::default());
    let base = [
        view("v"),
        lathe("l", 500, 24),
        cube("c0", ""),
        cube("c1", ""),
        cube("c2", ""),
    ];
    commit(&mut state, &mut global, &put("a", &base));
    assert_eq!(state.usage().triangles, 23_988);
    for i in 0..10 {
        let pair = [cube(&format!("x{i}"), ""), cube(&format!("y{i}"), "")];
        assert_eq!(
            refused(&state, &global, &put(&format!("no{i}"), &pair)),
            over(Budget::Triangles)
        );
    }
    commit(&mut state, &mut global, &put("fits", &[cube("x0", "")]));
    assert_eq!(
        (state.usage().triangles, global.usage().triangles),
        (24_000, 24_000)
    );
}

#[test]
fn global_triangles_stop_at_24000_across_visible_scenes() {
    let mut global = GlobalBudget::default();
    let mut scenes: Vec<SceneState> = (0..4).map(|_| scene()).collect();
    for state in &mut scenes {
        commit(
            state,
            &mut global,
            &put("a", &[view("v"), lathe("l", 126, 24)]),
        );
    }
    assert_eq!(global.usage().triangles, 24_000);
    assert_eq!(
        refused(&scenes[2], &global, &put("b", &[cube("c", "")])),
        over(Budget::Triangles)
    );

    let mut global = GlobalBudget::default();
    let mut bindings: Vec<SceneBinding> = (0..4)
        .map(|_| SceneBinding::bind(Deferred::new(scene(), 0), &mut global).expect("binds"))
        .collect();
    for binding in &mut bindings {
        let read = binding.read();
        let candidate = read
            .admit_detached(&step(&put("a", &[view("v"), lathe("l", 126, 24)])))
            .expect("valid step");
        binding
            .apply_detached(read, candidate, &mut global)
            .expect("within the global cap");
    }
    let before = (
        bindings[1].view().usage(),
        global.usage(),
        global.generation(),
    );
    let read = bindings[1].read();
    let candidate = read
        .admit_detached(&step(&put("b", &[cube("c", "")])))
        .expect("detached admission is local only");
    assert!(matches!(
        bindings[1].apply_detached(read, candidate, &mut global),
        Err(BindingError::OverBudget(ErrorCode::OverBudget(
            Budget::Triangles
        )))
    ));
    assert_eq!(
        before,
        (
            bindings[1].view().usage(),
            global.usage(),
            global.generation()
        )
    );
}

#[test]
fn one_view_per_scene_counts_a_pending_removal() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    assert_eq!(
        refused(&state, &global, &put("two", &[view("v"), view("w")])),
        over(Budget::Views)
    );
    commit(&mut state, &mut global, &put("a", &[view("v")]));
    let removal = commit(&mut state, &mut global, r#"{"id":"b","remove":["v"]}"#);
    assert_eq!(
        refused(&state, &global, &put("c", &[view("w")])),
        over(Budget::Views)
    );
    state
        .retire(removal.index, &mut global)
        .expect("committed step");
    commit(&mut state, &mut global, &put("c", &[view("w")]));
    assert_eq!(state.usage().views, 1);
}

#[test]
fn attachment_counts_toward_depth_16() {
    let mut ops = vec![view("v"), cube("s0", "")];
    for i in 1..15 {
        ops.push(cube(
            &format!("s{i}"),
            &format!(r#","attach":"s{}""#, i - 1),
        ));
    }
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(&mut state, &mut global, &put("deep", &ops));
    assert_eq!(
        refused(
            &state,
            &global,
            &put("more", &[cube("s15", r#","attach":"s14""#)])
        ),
        at(ErrorCode::TooDeep, "put")
    );
}

#[test]
fn attachment_names_an_earlier_solid_in_the_same_view() {
    let (state, global) = (scene(), GlobalBudget::default());
    let other =
        r#"{"solid":{"in":"w","id":"b","shape":"box","size":[1,1,1],"attach":"a"}}"#.to_owned();
    assert_eq!(
        refused(
            &state,
            &global,
            &put("x", &[view("v"), view("w"), cube("a", ""), other])
        ),
        at(ErrorCode::OutOfRange, "put[3].solid.attach")
    );
    assert_eq!(
        refused(
            &state,
            &global,
            &put(
                "x",
                &[view("v"), cube("a", r#","attach":"b""#), cube("b", "")]
            )
        ),
        at(ErrorCode::ForwardRef, "put[1].solid.attach")
    );
    assert_eq!(
        refused(
            &state,
            &global,
            &put("x", &[view("v"), cube("a", r#","attach":"z""#)])
        ),
        at(ErrorCode::UnknownRef, "put[1].solid.attach")
    );
    let mut state = state;
    let mut global = global;
    commit(
        &mut state,
        &mut global,
        &put(
            "a",
            &[view("v"), cube("a", ""), cube("b", r#","attach":"a""#)],
        ),
    );
    assert_eq!(
        refused(&state, &global, r#"{"id":"r","remove":["a"]}"#),
        at(ErrorCode::Orphans, "remove[0]")
    );
    assert_eq!(
        refused(
            &state,
            &global,
            r#"{"id":"t","put":[{"text":{"id":"t","text":"","in":"v","at":[0,0]}}]}"#
        ),
        at(ErrorCode::OutOfRange, "put[0].text.in")
    );
}

#[test]
fn a_3d_track_limits_its_view_to_6000_triangles() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put("a", &[view("v"), lathe("l", 126, 24)]),
    );
    commit(
        &mut state,
        &mut global,
        &put("b", &[track("l", "pos3", [1.0, 0.0, 0.0], "linear")]),
    );
    assert_eq!(
        refused(&state, &global, &put("c", &[cube("c", "")])),
        over(Budget::MovingTriangles)
    );

    for kind in ["pos3", "rot3"] {
        let (mut state, mut global) = (scene(), GlobalBudget::default());
        commit(
            &mut state,
            &mut global,
            &put("a", &[view("v"), lathe("l", 126, 24), cube("c", "")]),
        );
        assert_eq!(
            refused(
                &state,
                &global,
                &put("b", &[track("c", kind, [1.0, 0.0, 0.0], "linear")])
            ),
            over(Budget::MovingTriangles),
            "{kind}"
        );
    }

    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put(
            "a",
            &[
                view("v"),
                cube("m", ""),
                track("m", "rot3", [0.0, 90.0, 0.0], "linear"),
            ],
        ),
    );
    assert_eq!(
        refused(&state, &global, &put("b", &[lathe("l", 126, 24)])),
        over(Budget::MovingTriangles)
    );
    let removal = commit(&mut state, &mut global, r#"{"id":"r","remove":["m"]}"#);
    assert_eq!(
        refused(&state, &global, &put("b", &[lathe("l", 126, 24)])),
        over(Budget::MovingTriangles),
        "a pending removal still holds its triangles"
    );
    state
        .retire(removal.index, &mut global)
        .expect("committed step");
    commit(&mut state, &mut global, &put("b", &[lathe("l", 126, 24)]));
    assert_eq!(state.usage().triangles, 6_000);
}

#[test]
fn a_pos3_spring_bound_includes_its_overshoot() {
    let (state, global) = (scene(), GlobalBudget::default());
    let linear = put(
        "a",
        &[
            view("v"),
            cube("s", ""),
            track("s", "pos3", [19_000.0, 0.0, 0.0], "linear"),
        ],
    );
    let spring = linear.replace("linear", "spring");
    state
        .admit(&step(&linear), &global)
        .expect("keys inside the bound");
    assert_eq!(
        refused(&state, &global, &spring),
        at(ErrorCode::OutOfRange, "put")
    );

    let parent = |ease: &str| {
        put(
            "a",
            &[
                view("v"),
                cube("p", ""),
                cube("c", r#","attach":"p","pos":[4000,0,0]"#),
                track("p", "pos3", [12_000.0, 0.0, 0.0], ease),
            ],
        )
    };
    state
        .admit(&step(&parent("linear")), &global)
        .expect("16,001.7");
    assert_eq!(
        refused(&state, &global, &parent("spring")),
        at(ErrorCode::OutOfRange, "put")
    );

    let from_rest = |ease: &str| {
        put(
            "a",
            &[
                view("v"),
                cube("s", ""),
                format!(
                    r#"{{"anim":{{"target":"s","track":"pos3","keys":[[500,[14600,0,0]]],"ease":"{ease}"}}}}"#
                ),
            ],
        )
    };
    state
        .admit(&step(&from_rest("linear")), &global)
        .expect("14,601.7");
    assert_eq!(
        refused(&state, &global, &from_rest("spring")),
        at(ErrorCode::OutOfRange, "put")
    );
}

#[test]
fn a_yoyo_spring_bound_includes_the_reverse_overshoot() {
    let (state, global) = (scene(), GlobalBudget::default());
    let swing = |ease: &str, yoyo: bool| {
        put(
            "a",
            &[
                view("v"),
                cube("s", ""),
                format!(
                    r#"{{"anim":{{"target":"s","track":"pos3","keys":[[0,[14000,0,0]],[1000,[-2000,0,0]]],"ease":"{ease}","repeat":1,"yoyo":{yoyo}}}}}"#
                ),
            ],
        )
    };
    state
        .admit(&step(&swing("spring", false)), &global)
        .expect("19,264.3 forward");
    state
        .admit(&step(&swing("linear", true)), &global)
        .expect("14,001.7 both ways");
    assert_eq!(
        refused(&state, &global, &swing("spring", true)),
        at(ErrorCode::OutOfRange, "put")
    );
}

#[test]
fn a_replaced_spring_track_bounds_its_view_until_retirement() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put(
            "a",
            &[
                view("v"),
                cube("s", ""),
                track("s", "pos3", [14_000.0, 0.0, 0.0], "spring"),
            ],
        ),
    );
    let replaced = commit(
        &mut state,
        &mut global,
        &put("b", &[track("s", "pos3", [1.0, 0.0, 0.0], "linear")]),
    );
    let child = put("c", &[cube("c", r#","attach":"s","pos":[3000,0,0]"#)]);
    assert_eq!(
        refused(&state, &global, &child),
        at(ErrorCode::OutOfRange, "put")
    );
    state
        .retire(replaced.index, &mut global)
        .expect("committed step");
    commit(&mut state, &mut global, &child);
}

#[test]
fn a_track_replaced_in_the_same_step_keeps_its_old_spring_envelope() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put(
            "a",
            &[
                view("v"),
                cube("s", ""),
                track("s", "pos3", [14_000.0, 0.0, 0.0], "spring"),
            ],
        ),
    );
    let replace = put(
        "b",
        &[
            track("s", "pos3", [1.0, 0.0, 0.0], "linear"),
            cube("c", r#","attach":"s","pos":[3000,0,0]"#),
        ],
    );
    assert_eq!(
        refused(&state, &global, &replace),
        at(ErrorCode::OutOfRange, "put")
    );
}

#[test]
fn a_view_box_needs_a_positive_width_and_height() {
    let (state, global) = (scene(), GlobalBudget::default());
    let boxed = |bounds: &str| {
        put(
            "a",
            &[format!(r#"{{"view3d":{{"id":"v","box":[{bounds}]}}}}"#)],
        )
    };
    state
        .admit(&step(&boxed("0,0,1,1")), &global)
        .expect("a one-unit view");
    for (bounds, path) in [
        ("0,0,0,10", "put[0].view3d.box[2]"),
        ("0,0,-1,10", "put[0].view3d.box[2]"),
        ("0,0,10,0", "put[0].view3d.box[3]"),
        ("0,0,10,-1", "put[0].view3d.box[3]"),
    ] {
        assert_eq!(
            refused(&state, &global, &boxed(bounds)),
            at(ErrorCode::OutOfRange, path),
            "{bounds}"
        );
    }
}

#[test]
fn the_composed_extent_stops_at_20000_and_ignores_cancellation() {
    let (state, global) = (scene(), GlobalBudget::default());
    let placed = |x: &str| {
        put(
            "a",
            &[view("v"), sphere("s", &format!(r#","pos":[{x},0,0]"#))],
        )
    };
    state
        .admit(&step(&placed("19999")), &global)
        .expect("exactly 20,000");
    assert_eq!(
        refused(&state, &global, &placed("19999.002")),
        at(ErrorCode::OutOfRange, "put")
    );
    assert_eq!(
        refused(
            &state,
            &global,
            &put("a", &[view("v"), sphere("s", r#","pos":[15000,15000,0]"#)])
        ),
        at(ErrorCode::OutOfRange, "put")
    );
    assert_eq!(
        refused(
            &state,
            &global,
            &put(
                "a",
                &[
                    view("v"),
                    sphere("p", r#","pos":[15000,0,0]"#),
                    sphere("c", r#","attach":"p","pos":[-15000,0,0]"#),
                ]
            )
        ),
        at(ErrorCode::OutOfRange, "put")
    );
}

#[test]
fn sizing_refuses_degenerate_shapes_before_commit() {
    let (state, global) = (scene(), GlobalBudget::default());
    let axis = r#"{"solid":{"in":"v","id":"s","shape":"lathe","points":[[0,0],[0,1]]}}"#;
    assert_eq!(
        refused(&state, &global, &put("a", &[view("v"), axis.to_owned()])),
        at(ErrorCode::Degenerate, "put[1].solid.points")
    );
    let bowtie = r#"{"solid":{"in":"v","id":"s","shape":"extrude","points":[[0,0],[1,1],[1,0],[0,1]],"depth":1}}"#;
    assert_eq!(
        refused(&state, &global, &put("a", &[view("v"), bowtie.to_owned()])),
        at(ErrorCode::Degenerate, "put[1].solid.points")
    );
}

#[test]
fn a_removed_solid_releases_triangles_only_at_retirement() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put("a", &[view("v"), cube("c", "")]),
    );
    let removal = commit(&mut state, &mut global, r#"{"id":"b","remove":["c"]}"#);
    assert_eq!(
        (state.usage().triangles, global.usage().triangles),
        (12, 12)
    );
    state.retire(removal.index, &mut global).expect("committed");
    assert_eq!((state.usage().triangles, global.usage().triangles), (0, 0));
    let generation = global.generation();
    state.retire(removal.index, &mut global).expect("repeat");
    assert_eq!(
        (state.usage().triangles, global.generation()),
        (0, generation)
    );
}

#[test]
fn sets_anchors_and_tracks_respect_3d_ownership() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put(
            "a",
            &[
                view("v"),
                cube("s", ""),
                r#"{"text":{"id":"t","text":"","at":[0,0]}}"#.to_owned(),
            ],
        ),
    );
    commit(
        &mut state,
        &mut global,
        r##"{"id":"b","set":[{"target":"s","label":"lid","info":"opens","color":"#ff0000"}]}"##,
    );
    for (json, expected) in [
        (
            r#"{"id":"c","set":[{"target":"s","text":"x"}]}"#,
            at(ErrorCode::KindChanged, "set[0].text"),
        ),
        (
            r##"{"id":"c","set":[{"target":"v","color":"#ff0000"}]}"##,
            at(ErrorCode::KindChanged, "set[0].color"),
        ),
        (
            r#"{"id":"c","set":[{"target":"t","info":"x"}]}"#,
            at(ErrorCode::KindChanged, "set[0].info"),
        ),
        (
            r#"{"id":"c","put":[{"text":{"id":"u","text":"","at":{"ref":"s"}}}]}"#,
            at(ErrorCode::Unsupported, "put[0].text.at.ref"),
        ),
        (
            r#"{"id":"c","put":[{"anim":{"target":"s","track":"opacity","keys":[[0,1]]}}]}"#,
            at(ErrorCode::PropertyNotOwned, "put[0].anim.track"),
        ),
        (
            r#"{"id":"c","put":[{"anim":{"target":"t","track":"pos3","keys":[[0,[1,2,3]]]}}]}"#,
            at(ErrorCode::PropertyNotOwned, "put[0].anim.track"),
        ),
        (
            r#"{"id":"c","put":[{"anim":{"target":"v","track":"rot3","keys":[[0,[1,2,3]]]}}]}"#,
            at(ErrorCode::PropertyNotOwned, "put[0].anim.track"),
        ),
    ] {
        assert_eq!(refused(&state, &global, json), expected, "{json}");
    }
}

#[test]
fn stop_keeps_a_3d_object_only_with_its_view_and_attachment_chain() {
    let fixture = |global: &mut GlobalBudget| {
        let mut state = scene();
        commit(
            &mut state,
            global,
            &put(
                "a",
                &[
                    view("v"),
                    cube("a", ""),
                    r#"{"text":{"id":"t","text":"","at":[0,0]}}"#.to_owned(),
                ],
            ),
        );
        commit(
            &mut state,
            global,
            &put("b", &[cube("b", r#","attach":"a""#)]),
        );
        state
    };
    let handle = |revision, slot| Handle::new(revision, slot).expect("handle");
    let cut = |revealed: Vec<Handle>| StopCut {
        activated: 2,
        revealed: Bounded::new(revealed).expect("bounded"),
    };

    let mut global = GlobalBudget::default();
    let mut state = fixture(&mut global);
    let outcome = state
        .stop(
            &cut(vec![handle(1, 0), handle(1, 2), handle(2, 0)]),
            &mut global,
        )
        .expect("valid cut");
    assert_eq!(outcome, StopOutcome::Kept);
    assert_eq!(
        ["v", "a", "t", "b"].map(|name| state.handle(&id(name)).is_some()),
        [true, false, true, false],
        "b leaves with its unrevealed attachment ancestor"
    );
    assert_eq!((state.usage().solids, state.usage().triangles), (0, 0));

    let mut global = GlobalBudget::default();
    let mut state = fixture(&mut global);
    state
        .stop(
            &cut(vec![handle(1, 0), handle(1, 1), handle(1, 2), handle(2, 0)]),
            &mut global,
        )
        .expect("valid cut");
    assert_eq!(state.object_count(), 4);
    assert_eq!((state.usage().solids, state.usage().triangles), (2, 24));

    let mut global = GlobalBudget::default();
    let mut state = fixture(&mut global);
    state
        .stop(
            &cut(vec![handle(1, 1), handle(1, 2), handle(2, 0)]),
            &mut global,
        )
        .expect("valid cut");
    assert_eq!(
        ["v", "a", "t", "b"].map(|name| state.handle(&id(name)).is_some()),
        [false, false, true, false],
        "solids leave with their unrevealed view"
    );
}

#[test]
fn reach_bounds_every_generated_vertex_from_the_local_origin() {
    for json in [
        r#"{"in":"v","id":"s","shape":"box","size":[2,4,6]}"#,
        r#"{"in":"v","id":"s","shape":"sphere","r":3}"#,
        r#"{"in":"v","id":"s","shape":"cylinder","r":2,"h":5}"#,
        r#"{"in":"v","id":"s","shape":"cone","r":2,"r2":1,"h":3}"#,
        r#"{"in":"v","id":"s","shape":"torus","r":3,"r2":1}"#,
        r#"{"in":"v","id":"s","shape":"extrude","points":[[10,10],[12,10],[12,13],[10,13]],"depth":2}"#,
        r#"{"in":"v","id":"s","shape":"lathe","points":[[1,5],[2,7],[1,9]]}"#,
        r#"{"in":"v","id":"s","shape":"tube","path":[[5,5,5],[8,5,5],[8,9,5]],"r2":0.5}"#,
    ] {
        let parsed = step(&format!(r#"{{"id":"x","put":[{{"solid":{json}}}]}}"#));
        let Op::Solid(solid) = &parsed.put[0] else {
            panic!("solid fixture");
        };
        let count = usize::try_from(triangles(solid).expect("valid")).expect("fits");
        let mut out = Vec::with_capacity(count);
        build(
            solid,
            &Transform::local([0.0; 3], [0.0; 3], 1.0),
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut MeshScratch::new(),
        )
        .expect("representable");
        let farthest = out
            .iter()
            .flat_map(|t| t.v)
            .map(|v| {
                [v.x, v.y, v.z]
                    .map(|c| f64::from(c).powi(2))
                    .iter()
                    .sum::<f64>()
                    .sqrt()
            })
            .fold(0.0, f64::max);
        let bound = reach(solid).expect("valid");
        let rounding = 1.0 + f64::from(f32::EPSILON);
        assert!(farthest <= bound * rounding, "{json}: {farthest} > {bound}");
        assert!(farthest > 0.0);
    }
}

#[test]
fn info_is_bounded_and_redacted_like_other_text() {
    let marker = "SENTINEL";
    let long = "é".repeat(400);
    let parsed = step(&format!(
        r#"{{"id":"x","set":[{{"target":"s","info":"{marker}{}"}}]}}"#,
        &long[..long.len() - 2 * marker.len()]
    ));
    let decimal = format!("{:?}", marker.as_bytes());
    for debug in [format!("{parsed:?}"), format!("{:?}", parsed.set[0])] {
        assert!(!debug.contains(marker), "{debug}");
        assert!(!debug.contains(decimal.trim_matches(['[', ']'])), "{debug}");
    }
    let over = format!(
        r#"{{"step":{{"id":"x","set":[{{"target":"s","info":"{}"}}]}}}}"#,
        "é".repeat(401)
    );
    let error = parse_record(over.as_bytes()).expect_err("401 characters");
    assert_eq!(error.code, ErrorCode::TooLong);
}

#[test]
fn set_info_keeps_its_utf8_text_through_a_stored_replay() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    commit(
        &mut state,
        &mut global,
        &put("a", &[view("v"), cube("s", "")]),
    );
    commit(
        &mut state,
        &mut global,
        r#"{"id":"b","set":[{"target":"s","info":"é½中"}]}"#,
    );
    let stored = serde_json::to_vec(state.recovery_log()).expect("serializable log");
    let log: Vec<SceneEvent> = serde_json::from_slice(&stored).expect("stored log");
    let replayed = SceneState::replay(header(), &log).expect("replay");
    assert_eq!(replayed.usage(), state.usage());
    let infos: Vec<&str> = replayed
        .recovery_log()
        .iter()
        .filter_map(|event| match event {
            SceneEvent::Step { step, .. } => step
                .set
                .first()?
                .info
                .as_ref()
                .map(kumochi_scene::Text::as_str),
            SceneEvent::Stop(_) => None,
        })
        .collect();
    assert_eq!(infos, ["é½中"]);
}

/// `tests/mesh_size.rs`'s maximum outline: 64 retained perimeter points, 252 triangles.
fn compact(name: &str) -> String {
    let points: Vec<String> = (0..16)
        .map(|i| format!("[{i},0]"))
        .chain((0..16).map(|i| format!("[16,{i}]")))
        .chain((0..16).map(|i| format!("[{},16]", 16 - i)))
        .chain((0..16).map(|i| format!("[0,{}]", 16 - i)))
        .collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"extrude","points":[{}],"depth":2}}}}"#,
        points.join(",")
    )
}

#[test]
fn a_step_admits_at_most_2304_extrude_outline_points() {
    let ops = |count: usize| {
        let mut ops = vec![view("v")];
        ops.extend((0..count).map(|i| compact(&format!("e{i}"))));
        put("a", &ops)
    };
    let record = |count| format!(r#"{{"step":{}}}"#, ops(count));
    assert_eq!(record(63).len(), 32_358);
    assert_eq!(record(64).len(), 32_871);
    assert_eq!(
        parse_record(record(64).as_bytes())
            .expect_err("over 32 KiB")
            .code,
        ErrorCode::TooLong
    );
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    assert_eq!(
        refused(&state, &global, &ops(63)),
        over(Budget::OutlinePoints)
    );
    assert_eq!(
        refused(&state, &global, &ops(37)),
        over(Budget::OutlinePoints)
    );
    commit(&mut state, &mut global, &ops(36));
    let more: Vec<String> = (36..72).map(|i| compact(&format!("e{i}"))).collect();
    commit(&mut state, &mut global, &put("b", &more));
    assert_eq!(
        (state.usage().solids, state.usage().triangles),
        (72, 18_144)
    );
}

#[test]
fn a_step_of_2305_extrude_outline_points_is_refused() {
    let extrude = |name: &str, points: &[String]| {
        format!(
            r#"{{"solid":{{"in":"v","id":"{name}","shape":"extrude","points":[{}],"depth":2}}}}"#,
            points.join(",")
        )
    };
    let round: Vec<String> = (0..62)
        .map(|i| {
            let angle = f64::from(i) * std::f64::consts::TAU / 62.0;
            format!("[{:.0},{:.0}]", 1000.0 * angle.cos(), 1000.0 * angle.sin())
        })
        .collect();
    let triangle = ["[0,0]".to_owned(), "[16,0]".to_owned(), "[0,16]".to_owned()];
    let mut ops = vec![view("v")];
    ops.extend((0..35).map(|i| compact(&format!("e{i}"))));
    ops.push(extrude("r", &round));
    ops.push(extrude("t", &triangle));
    assert_eq!(35 * 64 + round.len() + triangle.len(), 2_305);
    let (state, global) = (scene(), GlobalBudget::default());
    assert_eq!(
        refused(&state, &global, &put("a", &ops)),
        over(Budget::OutlinePoints)
    );
}

fn polygon(name: &str) -> String {
    let points: Vec<String> = (0..64)
        .map(|i| {
            let angle = f64::from(i) * std::f64::consts::TAU / 64.0;
            format!("[{:.0},{:.0}]", 1000.0 * angle.cos(), 1000.0 * angle.sin())
        })
        .collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"extrude","points":[{}],"depth":2}}}}"#,
        points.join(",")
    )
}

#[test]
#[ignore = "release CPU timing on a quiet machine with no concurrent builds"]
fn measure_admit() {
    use kumochi_scene::limits::MAX_STEP_OUTLINE_POINTS;
    use std::{hint::black_box, time::Instant};

    let extrudes = |outline: fn(&str) -> String, first: Option<String>| {
        let mut ops: Vec<String> = first.into_iter().collect();
        let mut count = 0;
        loop {
            ops.push(outline(&format!("e{count}")));
            if count == MAX_STEP_OUTLINE_POINTS / 64
                || format!(r#"{{"step":{}}}"#, put("a", &ops)).len() > 32 * 1024
            {
                ops.pop();
                return (count, step(&put("a", &ops)));
            }
            count += 1;
        }
    };
    let (count, largest) = extrudes(polygon, Some(view("v")));
    let (_, late) = extrudes(polygon, None);
    let (compact_count, compact_largest) = extrudes(compact, Some(view("v")));
    let (_, compact_late) = extrudes(compact, None);
    let mut full = (scene(), GlobalBudget::default());
    commit(
        &mut full.0,
        &mut full.1,
        &put("f", &[view("v"), lathe("l", 251, 48)]),
    );
    let mut boxes = vec![view("v")];
    boxes.extend((0..160).map(|i| cube(&format!("s{i}"), "")));
    let empty = (scene(), GlobalBudget::default());
    println!(
        "largest legal steps: {count} round and {compact_count} compact extrudes of 64 points"
    );
    for (workload, (state, global), step, refused) in [
        ("largest-legal-extrude-step", &empty, largest, None),
        (
            "extrude-step-refused-late",
            &full,
            late,
            Some(ErrorCode::OverBudget(Budget::Triangles)),
        ),
        (
            "largest-compact-extrude-step",
            &empty,
            compact_largest,
            None,
        ),
        (
            "compact-step-refused-late",
            &full,
            compact_late,
            Some(ErrorCode::OverBudget(Budget::Triangles)),
        ),
        ("160-small-solids", &empty, step(&put("a", &boxes)), None),
    ] {
        assert_eq!(
            state.admit(&step, global).err().map(|error| error.code),
            refused,
            "{workload}"
        );
        let mut samples: Vec<f64> = (0..200)
            .map(|_| {
                let start = Instant::now();
                drop(black_box(state.admit(black_box(&step), global)));
                start.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        println!(
            "{workload}: n=200 p50={:.1}us p99={:.1}us max={:.1}us",
            samples[100], samples[197], samples[199]
        );
    }
}

#[test]
fn a_solid_degenerate_only_in_the_world_refuses_before_commit() {
    let (mut state, mut global) = (scene(), GlobalBudget::default());
    let pair = |pos: &str| {
        [
            view("v"),
            cube("p", &format!(r#","pos":{pos},"scale":0.001"#)),
            cube("c", r#","attach":"p","scale":0.001"#),
        ]
    };
    assert_eq!(
        refused(&state, &global, &put("far", &pair("[1000,0,0]"))),
        at(ErrorCode::Degenerate, "put[2]"),
        "the child collapses only under its parent's scale"
    );
    commit(&mut state, &mut global, &put("near", &pair("[0,0,0]")));
    assert_eq!(
        refused(
            &state,
            &global,
            &format!(
                r#"{{"id":"moved","put":[{}]}}"#,
                track("p", "pos3", [1000.0, 0.0, 0.0], "linear")
            )
        ),
        at(ErrorCode::Degenerate, "put[0]"),
        "the child collapses only at the track's final offset"
    );
    commit(
        &mut state,
        &mut global,
        &format!(
            r#"{{"id":"moved","put":[{}]}}"#,
            track("p", "pos3", [0.5, 0.0, 0.0], "linear")
        ),
    );
    assert_eq!(
        state.revision(),
        2,
        "the next step admits under the refused id"
    );
}

#[test]
fn checked_admission_builds_mesh_scratch_only_for_a_view_it_checks() {
    // MeshScratch's largest buffer: 512 path points x 48 curved segments of [u32; 4] keys.
    const KEYS: usize = 512 * 48 * 16;
    let global = GlobalBudget::default();
    let text = r#"{"id":"flat","put":[{"text":{"id":"t","text":"","at":[0,0]}}]}"#;
    let (admission, freed) =
        allocation::releases_of_size(KEYS, || scene().admit_checked(&step(text), &global));
    assert!(matches!(admission, Ok(Admission::New(_))), "a 2D step admits");
    assert_eq!(freed, 0, "a step with no view to check builds no scratch");
    let solid = put("solid", &[view("v"), cube("c", "")]);
    let (admission, freed) =
        allocation::releases_of_size(KEYS, || scene().admit_checked(&step(&solid), &global));
    assert!(matches!(admission, Ok(Admission::New(_))), "a 3D step admits");
    assert_eq!(freed, 1, "calibration: checking one view builds one scratch");
}
