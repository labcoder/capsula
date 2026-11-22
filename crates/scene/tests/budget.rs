//! Expanded-work admission, retirement and final-result replay budgets.
use kumochi_scene::{
    Admission, Begin, Budget, ErrorCode, GlobalBudget, Handle, Record, ReplacementDisposition,
    SceneEvent, SceneState, Step, StepError, StopCut, WireId,
    budget::Usage,
    display::{Prim, WideMeasure},
    geom::Vec2,
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn header(name: &str) -> Begin {
    let Record::Begin(header) = parse_record(
        serde_json::to_string(&json!({"begin":{"v":1,"scene":name,"surface":"overlay","alt":""}}))
            .expect("fixture serializes")
            .as_bytes(),
    )
    .expect("typed header") else {
        panic!("begin fixture");
    };
    header
}
fn scene(name: &str) -> SceneState {
    SceneState::begin(header(name)).expect("valid header")
}
fn id(name: &str) -> WireId {
    name.to_owned().try_into().expect("valid id")
}
fn step(value: Value) -> Step {
    let envelope = serde_json::Map::from_iter([("step".to_owned(), value)]);
    let bytes = serde_json::to_vec(&envelope).expect("fixture serializes");
    let Record::Step(step) = parse_record(&bytes).expect("bounded typed fixture") else {
        panic!("step fixture");
    };
    step
}
fn candidate(
    state: &SceneState,
    global: &GlobalBudget,
    value: Value,
) -> Result<Admission, StepError> {
    state.admit(&step(value), global)
}
fn apply(state: &mut SceneState, global: &mut GlobalBudget, value: Value) {
    let Admission::New(admitted) = candidate(state, global, value).expect("legal resource fixture")
    else {
        panic!("new fixture");
    };
    state.commit(admitted, global).expect("fresh candidate");
}
fn put(state: &mut SceneState, global: &mut GlobalBudget, ops: Vec<Value>) {
    let value = Value::Object(serde_json::Map::from_iter([
        (
            "id".to_owned(),
            Value::String(format!("s{}", state.revision() + 1)),
        ),
        ("put".to_owned(), Value::Array(ops)),
    ]));
    apply(state, global, value);
}
fn texts(from: usize, count: usize) -> Vec<Value> {
    (from..from + count)
        .map(|n| json!({"text":{"id":format!("t{n}"),"at":[0,0],"text":""}}))
        .collect()
}
fn rejected(state: &SceneState, global: &GlobalBudget, value: Value, code: ErrorCode) -> StepError {
    let before = (
        state.revision(),
        state.usage(),
        global.usage(),
        global.generation(),
    );
    let error = candidate(state, global, value).expect_err("over-budget fixture");
    assert_eq!(error.code, code, "exact resource rejection");
    assert_eq!(
        (
            state.revision(),
            state.usage(),
            global.usage(),
            global.generation()
        ),
        before,
        "failed admission consumes nothing"
    );
    error
}
fn cut(revision: u32, handles: Vec<Handle>) -> StopCut {
    StopCut {
        activated: revision,
        revealed: Bounded::new(handles).expect("bounded revealed"),
    }
}

fn assert_polygon_picture(picture: &LayoutState, revision: u32) {
    let expected = Handle::new(revision, 0).expect("polygon handle");
    assert_eq!(
        picture.revision(),
        revision,
        "the ticket prepares the requested revision"
    );
    assert_eq!(
        picture.handle(&id("p")),
        Some(expected),
        "the ticket publishes the polygon"
    );
    let bounds = picture
        .bounds(&id("p"))
        .expect("valid bounds")
        .expect("polygon");
    assert_eq!(
        bounds.size,
        Vec2::new(40.0, 30.0),
        "the polygon keeps its authored dimensions"
    );
    let item = picture
        .items()
        .into_iter()
        .find(|item| item.handle == expected)
        .expect("painted polygon");
    let fills: Vec<Vec<Vec2>> = item
        .primitives
        .iter()
        .filter_map(|prim| match prim {
            Prim::Fill { points, color } if color.opacity > 0.0 => {
                Some(points.iter().map(|p| item.transform.point(*p)).collect())
            }
            _ => None,
        })
        .collect();
    for vertex in [
        Vec2::new(0.0, 0.0),
        Vec2::new(40.0, 0.0),
        Vec2::new(20.0, 30.0),
    ] {
        assert!(
            fills.iter().any(|points| points.contains(&vertex)),
            "the polygon paints its authored vertex {vertex:?}"
        );
    }
    for point in [
        Vec2::new(10.0, 5.0),
        Vec2::new(20.0, 14.0),
        Vec2::new(30.0, 5.0),
    ] {
        assert!(
            fills.iter().any(|points| {
                points
                    .iter()
                    .zip(points.iter().cycle().skip(1))
                    .filter(|(a, b)| {
                        (a.y > point.y) != (b.y > point.y)
                            && point.x < a.x + (b.x - a.x) * (point.y - a.y) / (b.y - a.y)
                    })
                    .count()
                    % 2
                    != 0
            }),
            "the polygon paints its interior at {point:?}"
        );
    }
}

#[test]
fn a_preview_is_the_ticket_its_commit_returns_and_refuses_a_stale_candidate() {
    let mut global = GlobalBudget::default();
    let mut old = scene("old");
    let prepare_ticket = |ticket, layout| {
        let input = PrepareInput::new(
            ticket,
            layout,
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .map_err(|(error, _)| error)
        .expect("ticket matches layout");
        prepare(input, &mut WideMeasure, &AtomicBool::new(false))
            .expect("ticket prepares")
            .after
    };
    let layout = LayoutState::new(&old);
    let Admission::New(first) =
        candidate(&old, &global, json!({"id":"one","put":texts(0,1)})).expect("first step")
    else {
        panic!("new first step");
    };
    let first = old.commit(first, &mut global).expect("first commit");
    let before = prepare_ticket(first.preparation, layout);
    let prepares_polygon = |ticket, layout, revision| {
        assert_polygon_picture(&prepare_ticket(ticket, layout), revision);
    };
    let ops =
        json!([{"shape":{"id":"p","kind":"polygon","fill":"red","points":[[0,0],[40,0],[20,30]]}}]);
    let Admission::New(admitted) =
        candidate(&old, &global, json!({"id":"two","put":ops})).expect("legal step")
    else {
        panic!("new step");
    };
    let Admission::New(rival) =
        candidate(&old, &global, json!({"id":"rival","put":texts(1, 1)})).expect("legal step")
    else {
        panic!("new step");
    };
    let preview = old.preview(&admitted).expect("a current candidate");
    let committed = old.commit(admitted, &mut global).expect("fresh candidate");
    assert_eq!(committed.revision, 2);
    prepares_polygon(preview, before.clone(), 2);
    prepares_polygon(committed.preparation, before, 2);
    assert_eq!(
        old.preview(&rival).expect_err("the scene moved past it"),
        kumochi_scene::StaleAdmission
    );

    let first = step(json!({"id":"first","put":ops}));
    let victim = (old.handle(&id("t0")), old.handle(&id("p")), old.revision());
    let usage = global.usage();
    let admission =
        kumochi_scene::ReplacementAdmission::checked(scene("next"), &first, &[&old], &global)
            .expect("valid admission");
    let (ticket, layout) = admission.preview();
    prepares_polygon(ticket, layout.clone(), 1);
    assert_eq!(old.object_count(), 2, "a preview settles no victim");
    assert_eq!(
        (old.handle(&id("t0")), old.handle(&id("p")), old.revision()),
        victim
    );
    assert_eq!(global.usage(), usage);
    let (_, committed) = admission
        .settle(
            &mut [&mut old],
            &mut global,
            ReplacementDisposition::Install,
        )
        .expect("sealed transition")
        .expect("installed");
    prepares_polygon(committed.preparation, layout, 1);
}

#[test]
fn replacement_at_the_cap_is_owned_atomic_and_does_not_release_twice() {
    let mut global = GlobalBudget::default();
    let mut scenes: Vec<_> = (0..4).map(|n| scene(&format!("old{n}"))).collect();
    for state in &mut scenes {
        put(state, &mut global, texts(0, 1));
    }
    let next = scene("next");
    let first = step(json!({"id":"first","put":texts(0,1)}));
    assert_eq!(
        next.admit_checked(&first, &global)
            .expect_err("fifth scene")
            .code,
        ErrorCode::OverBudget(Budget::VisibleScenes)
    );
    let generation = global.generation();
    let Admission::New(peer) =
        candidate(&scenes[1], &global, json!({"id":"peer"})).expect("valid peer step")
    else {
        panic!("new peer step");
    };
    let admission = kumochi_scene::ReplacementAdmission::checked(
        next,
        &first,
        &[&scenes[0], &scenes[0]],
        &global,
    )
    .expect("the duplicated victim leaves once");
    assert_eq!(
        (
            global.usage().scenes,
            global.usage().objects,
            global.generation()
        ),
        (4, 4, generation)
    );
    assert_eq!(scenes[0].object_count(), 1);
    let (mut next, committed) = admission
        .settle(
            &mut [&mut scenes[0]],
            &mut global,
            ReplacementDisposition::Install,
        )
        .expect("sealed transition")
        .expect("installed");
    assert_eq!(committed.index, 0);
    assert!(scenes[0].is_cleared());
    assert_eq!((global.usage().scenes, global.usage().objects), (4, 4));
    let settled = global.generation();
    assert_eq!(
        scenes[1].commit(peer, &mut global),
        Err(kumochi_scene::StaleAdmission)
    );
    assert_eq!(
        (
            global.usage().scenes,
            global.usage().objects,
            global.generation()
        ),
        (4, 4, settled)
    );
    scenes[0].clear(&mut global).expect("already cleared");
    assert_eq!(
        (
            global.usage().scenes,
            global.usage().objects,
            global.generation()
        ),
        (4, 4, settled)
    );
    next.clear(&mut global).expect("new scene leaves");
    assert_eq!((global.usage().scenes, global.usage().objects), (3, 3));
}

#[test]
fn replacement_refusal_and_an_intervening_budget_change_preserve_the_victim() {
    let mut global = GlobalBudget::default();
    let mut old = scene("old");
    put(&mut old, &mut global, texts(0, 1));
    let first = step(json!({"id":"first","put":texts(0,1)}));
    let rejected = step(json!({"id":"bad","remove":["missing"]}));
    let usage = global.usage();
    let generation = global.generation();
    let first_handle = old.handle(&id("t0")).expect("original object");
    let original_log = old.recovery_log().to_vec();
    assert_eq!(
        kumochi_scene::ReplacementAdmission::checked(scene("bad"), &rejected, &[&old], &global)
            .expect_err("missing removal target")
            .code,
        ErrorCode::UnknownRef
    );
    assert_eq!(
        (old.object_count(), global.usage(), global.generation()),
        (1, usage, generation)
    );
    assert_eq!(old.handle(&id("t0")), Some(first_handle));
    assert_eq!(old.recovery_log(), original_log);
    let admission =
        kumochi_scene::ReplacementAdmission::checked(scene("next"), &first, &[&old], &global)
            .expect("valid admission");
    put(&mut old, &mut global, texts(1, 1));
    let changed = (global.usage(), global.generation());
    assert_eq!(
        changed.0,
        Usage {
            scenes: 1,
            objects: 2,
            ..Usage::default()
        }
    );
    let second_handle = old.handle(&id("t1")).expect("intervening object");
    let changed_log = old.recovery_log().to_vec();
    assert_eq!(
        admission
            .settle(
                &mut [&mut old],
                &mut global,
                ReplacementDisposition::Install
            )
            .expect_err("the intervening commit changed the budget"),
        kumochi_scene::StaleAdmission
    );
    assert_eq!(old.handle(&id("t0")), Some(first_handle));
    assert_eq!(old.handle(&id("t1")), Some(second_handle));
    assert_eq!(old.recovery_log(), changed_log);
    assert_eq!(
        (
            old.object_count(),
            old.is_cleared(),
            global.usage(),
            global.generation()
        ),
        (2, false, changed.0, changed.1)
    );
}

#[test]
fn suppressed_replacement_settles_only_the_frozen_victims() {
    let mut global = GlobalBudget::default();
    let mut old = scene("old");
    put(&mut old, &mut global, texts(0, 1));
    let first = step(json!({"id":"first","put":texts(0,1)}));
    let admission =
        kumochi_scene::ReplacementAdmission::checked(scene("next"), &first, &[&old], &global)
            .expect("valid admission");
    let generation = global.generation();
    assert!(
        admission
            .settle(
                &mut [&mut old],
                &mut global,
                ReplacementDisposition::Discard
            )
            .expect("suppressed")
            .is_none()
    );
    assert!(old.is_cleared());
    assert_eq!(global.usage(), Usage::default());
    let settled = global.generation();
    assert_ne!(settled, generation);
    old.clear(&mut global).expect("later sweep");
    assert_eq!(
        (global.usage(), global.generation()),
        (Usage::default(), settled)
    );
}

#[test]
fn replacement_reuses_victim_capacity_at_the_global_object_limit() {
    let mut global = GlobalBudget::default();
    let mut old = scene("old");
    let mut retained = scene("retained");
    let mut tail = scene("tail");
    for state in [&mut old, &mut retained] {
        put(state, &mut global, texts(0, 300));
        put(state, &mut global, texts(300, 300));
    }
    put(&mut tail, &mut global, texts(0, 300));
    assert_eq!(global.usage().objects, 1500);
    let new = scene("new");
    let first = step(json!({"id":"first","put":texts(0,300)}));
    assert_eq!(
        new.admit_checked(&first, &global)
            .expect_err("ordinary early check is full")
            .code,
        ErrorCode::OverBudget(Budget::LiveObjects)
    );
    let admitted = kumochi_scene::ReplacementAdmission::checked(new, &first, &[&tail], &global)
        .expect("replacement uses released capacity");
    let (new, _) = admitted
        .settle(
            &mut [&mut tail],
            &mut global,
            ReplacementDisposition::Install,
        )
        .expect("sealed")
        .expect("installed");
    assert_eq!(new.object_count(), 300);
    assert_eq!((global.usage().objects, global.usage().scenes), (1500, 3));
    assert_eq!(retained.object_count(), 600);
    assert!(tail.is_cleared());
}

#[test]
fn replacement_checks_every_victim_and_context_before_mutation() {
    let mut global = GlobalBudget::default();
    let mut one = scene("one");
    let mut two = scene("two");
    put(&mut one, &mut global, texts(0, 1));
    put(&mut two, &mut global, texts(0, 1));
    let before = (global.usage(), global.generation());
    let first = step(json!({"id":"first","put":texts(0,1)}));
    let admission =
        kumochi_scene::ReplacementAdmission::checked(scene("new"), &first, &[&one], &global)
            .expect("admission");
    assert_eq!(
        admission
            .settle(
                &mut [&mut two],
                &mut global,
                ReplacementDisposition::Install
            )
            .expect_err("wrong victim identity"),
        kumochi_scene::StaleAdmission
    );
    assert_eq!(
        (
            one.is_cleared(),
            two.is_cleared(),
            global.usage(),
            global.generation()
        ),
        (false, false, before.0, before.1)
    );
    let mut foreign = GlobalBudget::default();
    let mut foreign_scene = scene("foreign");
    put(&mut foreign_scene, &mut foreign, texts(0, 1));
    put(&mut foreign_scene, &mut foreign, texts(1, 1));
    assert_eq!(foreign.generation(), global.generation());
    let admission =
        kumochi_scene::ReplacementAdmission::checked(scene("new"), &first, &[&one], &global)
            .expect("admission");
    let foreign_before = (foreign.usage(), foreign.generation());
    assert_eq!(
        admission
            .settle(
                &mut [&mut one],
                &mut foreign,
                ReplacementDisposition::Install
            )
            .expect_err("wrong global identity"),
        kumochi_scene::StaleAdmission
    );
    assert_eq!((global.usage(), global.generation()), before);
    assert_eq!((foreign.usage(), foreign.generation()), foreign_before);
    assert!(!one.is_cleared());
    assert_eq!(
        kumochi_scene::ReplacementAdmission::checked(
            scene("new"),
            &first,
            &[&foreign_scene],
            &global
        )
        .expect_err("foreign victim")
        .code,
        ErrorCode::BudgetContext
    );
    assert_eq!(
        kumochi_scene::ReplacementAdmission::checked(scene("new"), &first, &[&one; 5], &global)
            .expect_err("victim bound")
            .code,
        ErrorCode::OverBudget(Budget::VisibleScenes)
    );
}

#[test]
fn pending_removals_count_until_shown_and_replay_does_not_invent_activation() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    put(&mut state, &mut global, texts(0, 300));
    put(&mut state, &mut global, texts(300, 300));
    assert_eq!(state.usage().objects, 600);
    rejected(
        &state,
        &global,
        json!({"id":"too_many","put":texts(600,1)}),
        ErrorCode::OverBudget(Budget::LiveObjects),
    );
    for cycle in 0..3 {
        let remove: Vec<_> = (cycle * 300..cycle * 300 + 300)
            .map(|n| format!("t{n}"))
            .collect();
        let step_id = format!("s{}", state.revision() + 1);
        apply(
            &mut state,
            &mut global,
            json!({"id":step_id,"remove":remove}),
        );
        assert_eq!((state.object_count(), state.usage().objects), (300, 600));
        for _ in 0..10 {
            rejected(
                &state,
                &global,
                json!({"id":"rejected","put":texts(600+cycle*300,300)}),
                ErrorCode::OverBudget(Budget::LiveObjects),
            );
        }
        state
            .retire(state.revision() - 1, &mut global)
            .expect("removal shown");
        assert_eq!(state.usage().objects, 300);
        put(&mut state, &mut global, texts(600 + cycle * 300, 300));
        assert_eq!(state.usage().objects, 600);
    }
    let replay = SceneState::replay(header("s"), state.recovery_log())
        .expect("unlogged retirement cannot reject recovery");
    assert_eq!(
        (replay.revision(), replay.usage()),
        (state.revision(), state.usage())
    );
    let newest = Some(Handle::new(8, 0).expect("last batch's first object"));
    assert_eq!(state.handle(&id("t1200")), newest);
    assert_eq!(replay.handle(&id("t1200")), newest);

    let mut stopped = scene("stopped");
    let mut budget = GlobalBudget::default();
    put(&mut stopped, &mut budget, texts(0, 1));
    put(&mut stopped, &mut budget, texts(1, 1));
    stopped
        .stop(
            &cut(1, vec![Handle::new(1, 0).expect("valid")]),
            &mut budget,
        )
        .expect("drop unactivated second step");
    let replay = SceneState::replay(header("stopped"), stopped.recovery_log())
        .expect("charge retirement does not raise activated");
    assert_eq!(
        (
            replay.revision(),
            replay.committed(),
            replay.usage().objects
        ),
        (2, 1, 1)
    );
}

#[test]
fn removal_charges_converge_when_step_shown_arrives_and_do_not_release_twice() {
    let mut brain = scene("s");
    let mut shell = scene("s");
    let mut a = GlobalBudget::default();
    let mut b = GlobalBudget::default();
    for state_and_budget in [(&mut brain, &mut a), (&mut shell, &mut b)] {
        put(state_and_budget.0, state_and_budget.1, texts(0, 300));
        apply(
            state_and_budget.0,
            state_and_budget.1,
            json!({"id":"remove","remove":(0..300).map(|n|format!("t{n}")).collect::<Vec<_>>()}),
        );
    }
    shell.retire(1, &mut b).expect("activation");
    assert_eq!((a.usage().objects, b.usage().objects), (300, 0));
    brain.retire(1, &mut a).expect("StepShown");
    assert_eq!(a.usage(), b.usage());
    let generation = a.generation();
    brain.retire(1, &mut a).expect("duplicate StepShown");
    assert_eq!(a.generation(), generation);
    brain.clear(&mut a).expect("clear");
    assert_eq!(a.usage(), kumochi_scene::budget::Usage::default());
}

#[test]
fn replay_checks_the_combined_final_result_not_another_scenes_historical_peak() {
    let mut old = scene("old");
    let mut b = scene("b");
    let mut c = scene("c");
    let mut global = GlobalBudget::default();
    put(&mut old, &mut global, texts(0, 300));
    put(&mut old, &mut global, texts(300, 300));
    apply(
        &mut old,
        &mut global,
        json!({"id":"remove","remove":(0..600).map(|n|format!("t{n}")).collect::<Vec<_>>()}),
    );
    old.retire(2, &mut global)
        .expect("old peak retired before other scenes");
    for state in [&mut b, &mut c] {
        put(state, &mut global, texts(0, 300));
        put(state, &mut global, texts(300, 300));
    }
    assert_eq!((global.usage().objects, global.usage().scenes), (1200, 3));
    let (mut restored, mut budget) = SceneState::replay_all(&[
        (b.header(), b.recovery_log()),
        (c.header(), c.recovery_log()),
        (old.header(), old.recovery_log()),
    ])
    .expect("reversed scene order must not charge 1800 historical objects");
    assert_eq!(budget.usage(), global.usage());
    put(&mut restored[2], &mut budget, texts(600, 300));
    assert_eq!(budget.usage().objects, 1500);
    rejected(
        &restored[2],
        &budget,
        json!({"id":"too_many","put":texts(900,1)}),
        ErrorCode::OverBudget(Budget::LiveObjects),
    );
    let invalid: Vec<_> = (0..3).map(|_| (b.header(), b.recovery_log())).collect();
    assert_eq!(
        SceneState::replay_all(&invalid)
            .expect_err("final total 1800 is rejected")
            .code,
        ErrorCode::OverBudget(Budget::LiveObjects)
    );
}

#[test]
fn four_scenes_fit_and_foreign_budget_mutations_are_rejected_without_corruption() {
    let mut budget = GlobalBudget::default();
    let mut scenes: Vec<_> = (0..4).map(|n| scene(&format!("s{n}"))).collect();
    for state in &mut scenes {
        put(state, &mut budget, texts(0, 1));
    }
    assert_eq!((budget.usage().scenes, budget.usage().objects), (4, 4));
    let fifth = scene("fifth");
    rejected(
        &fifth,
        &budget,
        json!({"id":"s1","put":texts(0,1)}),
        ErrorCode::OverBudget(Budget::VisibleScenes),
    );
    let mut foreign = GlobalBudget::default();
    let original = (budget.usage(), budget.generation());
    let foreign_before = (foreign.usage(), foreign.generation());
    let state = &mut scenes[0];
    let object = state.handle(&id("t0")).expect("live object");
    let before = (
        state.revision(),
        state.activated(),
        state.recovery_log().to_vec(),
    );
    assert_eq!(
        state
            .stop(&cut(0, vec![]), &mut foreign)
            .expect_err("foreign context")
            .code,
        ErrorCode::BudgetContext
    );
    assert_eq!(
        state
            .retire(0, &mut foreign)
            .expect_err("foreign context")
            .code,
        ErrorCode::BudgetContext
    );
    assert_eq!(
        state.clear(&mut foreign).expect_err("foreign context").code,
        ErrorCode::BudgetContext
    );
    assert_eq!((budget.usage(), budget.generation()), original);
    assert_eq!((foreign.usage(), foreign.generation()), foreign_before);
    assert_eq!(state.handle(&id("t0")), Some(object));
    assert_eq!(
        (state.revision(), state.activated(), state.recovery_log()),
        (before.0, before.1, before.2.as_slice())
    );
    let Admission::New(pending) = candidate(
        &scenes[0],
        &budget,
        json!({"id":"pending","put":texts(1,1)}),
    )
    .expect("candidate") else {
        panic!("new fixture");
    };
    scenes[0]
        .stop(&cut(0, vec![]), &mut budget)
        .expect("clear before activation");
    assert_eq!(
        scenes[0].commit(pending, &mut budget),
        Err(kumochi_scene::StaleAdmission)
    );
}

#[test]
fn shown_content_never_clears_via_stop_zero_and_same_step_updates_cannot_outlive_removal() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    put(&mut state, &mut global, texts(0, 1));
    state.retire(0, &mut global).expect("shown");
    let error = state
        .stop(&cut(0, vec![]), &mut global)
        .expect_err("user clear is a separate transition");
    assert_eq!(error.code, ErrorCode::OutOfRange);
    assert_eq!(error.path.as_str(), "stop.activated");
    let set = rejected(
        &state,
        &global,
        json!({"id":"bad","set":[{"target":"t0","text":"change"}],"remove":["t0"]}),
        ErrorCode::RemovedInStep,
    );
    assert_eq!(set.path.as_str(), "set[0].target");
    let anim = rejected(
        &state,
        &global,
        json!({"id":"bad","put":[{"anim":{"target":"t0","track":"opacity","keys":[[0,0],[1,1]]}}],"remove":["t0"]}),
        ErrorCode::RemovedInStep,
    );
    assert_eq!(anim.path.as_str(), "put[0].anim.target");
}

fn table_ops(prefix: &str, rows: usize) -> Vec<Value> {
    let mut ops =
        vec![json!({"table":{"id":format!("{prefix}table"),"cols":["","","","","","","",""]}})];
    ops.extend((0..rows).map(
        |n| json!({"row":{"id":format!("{prefix}r{n}"),"in":format!("{prefix}table"),"cells":[]}}),
    ));
    ops
}
#[test]
fn table_rows_are_charged_after_padding_and_global_cells_reach_the_exact_cap() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    put(&mut state, &mut global, table_ops("first", 40));
    assert_eq!(state.usage().cells, 320);
    rejected(
        &state,
        &global,
        json!({"id":"extra_row","put":[{"row":{"id":"extra","in":"firsttable","cells":[]}}]}),
        ErrorCode::OverBudget(Budget::TableCells),
    );
    for n in 1..12 {
        put(&mut state, &mut global, table_ops(&format!("a{n}"), 40));
    }
    put(&mut state, &mut global, table_ops("last", 20));
    put(
        &mut state,
        &mut global,
        vec![json!({"table":{"id":"narrow","cols":[""]}})],
    );
    assert_eq!((global.usage().cells, state.usage().objects), (4000, 514));
    rejected(
        &state,
        &global,
        json!({"id":"extra_cell","put":[{"row":{"id":"extra","in":"narrow","cells":[]}}]}),
        ErrorCode::OverBudget(Budget::TableCells),
    );
}

fn chart_ops(prefix: &str, series: usize, points: usize) -> Vec<Value> {
    let mut ops = vec![json!({"chart":{"id":format!("{prefix}chart"),"kind":"scatter"}})];
    ops.extend((0..series).map(|n|json!({"series":{"id":format!("{prefix}s{n}"),"in":format!("{prefix}chart"),"data":vec![1;points]}})));
    ops
}
#[test]
fn chart_series_marks_and_pending_data_replacements_are_preflighted() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    for n in 0..5 {
        put(&mut state, &mut global, chart_ops(&format!("a{n}"), 8, 500));
    }
    assert_eq!(global.usage().marks, 20000);
    rejected(
        &state,
        &global,
        json!({"id":"extra_series","put":[{"series":{"id":"extra","in":"a0chart","data":[]}}]}),
        ErrorCode::OverBudget(Budget::ChartMarks),
    );
    rejected(
        &state,
        &global,
        json!({"id":"extra_mark","put":chart_ops("extra",1,1)}),
        ErrorCode::OverBudget(Budget::ChartMarks),
    );
    apply(
        &mut state,
        &mut global,
        json!({"id":"shrink","set":[{"target":"a0s0","data":[]}]}),
    );
    assert_eq!(global.usage().marks, 20000);
    state
        .retire(state.revision() - 1, &mut global)
        .expect("shrink activated");
    assert_eq!(global.usage().marks, 19500);
    put(&mut state, &mut global, chart_ops("extra", 1, 500));
    assert_eq!(global.usage().marks, 20000);
}

fn long_path(curves: usize, lines: usize) -> String {
    format!(
        "{}{}",
        "M0 0 C0 20000 20000 20000 20000 0 ".repeat(curves),
        "L0 0 ".repeat(lines)
    )
}
#[test]
fn flattening_is_sized_before_vertices_and_all_segment_caps_are_exact() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    for n in 0..4 {
        put(
            &mut state,
            &mut global,
            vec![json!({"path":{"id":format!("p{n}"),"d":long_path(8,0)}})],
        );
    }
    assert_eq!(global.usage().segments, 16384);
    put(
        &mut state,
        &mut global,
        vec![json!({"path":{"id":"last","d":long_path(7,32)}})],
    );
    assert_eq!(global.usage().segments, 20000);
    rejected(
        &state,
        &global,
        json!({"id":"extra","put":[{"path":{"id":"extra","d":"M0 0 L1 0"}}]}),
        ErrorCode::OverBudget(Budget::PathSegments),
    );
    let empty = scene("empty");
    rejected(
        &empty,
        &GlobalBudget::default(),
        json!({"id":"too_flat","put":[{"path":{"id":"p","d":long_path(9,0)}}]}),
        ErrorCode::OverBudget(Budget::PathSegments),
    );
    let at_cap = format!("M0 0 {}", "L1 0 ".repeat(511));
    let mut cap = scene("cap");
    let mut budget = GlobalBudget::default();
    put(
        &mut cap,
        &mut budget,
        vec![json!({"path":{"id":"p","d":at_cap}})],
    );
    assert_eq!(budget.usage().segments, 511);
    rejected(
        &empty,
        &GlobalBudget::default(),
        json!({"id":"too_long","put":[{"path":{"id":"p","d":format!("M0 0 {}","L1 0 ".repeat(512))}}]}),
        ErrorCode::OverBudget(Budget::PathPoints),
    );
}

#[test]
fn tracks_depth_transforms_and_repeated_duration_are_bounded() {
    let mut state = scene("s");
    let mut global = GlobalBudget::default();
    put(&mut state, &mut global, texts(0, 257));
    for from in [0, 80, 160, 240] {
        let end = (from + 80).min(256);
        let tracks = (from..end)
            .map(|n| {
                json!({"anim":{"target":format!("t{n}"),"track":"opacity",
            "keys":(0..32).map(|time|json!([time,0])).collect::<Vec<_>>()}})
            })
            .collect();
        put(&mut state, &mut global, tracks);
    }
    assert_eq!((global.usage().tracks, global.usage().keys), (256, 8192));
    rejected(
        &state,
        &global,
        json!({"id":"extra","put":[{"anim":{"target":"t256","track":"opacity","keys":[[0,0],[100,1]]}}]}),
        ErrorCode::OverBudget(Budget::Tracks),
    );
    rejected(
        &state,
        &global,
        json!({"id":"repeat","put":[{"anim":{"target":"t0","track":"opacity","repeat":1,"keys":[[0,0],[60000,1]]}}]}),
        ErrorCode::OutOfRange,
    );

    let mut chain = scene("chain");
    let mut budget = GlobalBudget::default();
    let mut ops = vec![json!({"diagram":{"id":"g0"}})];
    ops.extend((1..16).map(|n| json!({"diagram":{"id":format!("g{n}"),"in":format!("g{}",n-1)}})));
    put(&mut chain, &mut budget, ops);
    rejected(
        &chain,
        &budget,
        json!({"id":"deep","put":[{"diagram":{"id":"g16","in":"g15"}}]}),
        ErrorCode::TooDeep,
    );
    let scales: Vec<_> = (0..16)
        .map(|n| json!({"anim":{"target":format!("g{n}"),"track":"scale","keys":[[0,20000]]}}))
        .collect();
    rejected(
        &chain,
        &budget,
        json!({"id":"overflow","put":scales}),
        ErrorCode::TransformOverflow,
    );
    let shrinking: Vec<_> = (0..16)
        .map(|n| json!({"anim":{"target":format!("g{n}"),"track":"scale","keys":[[0,1e-20]]}}))
        .collect();
    rejected(
        &chain,
        &budget,
        json!({"id":"underflow","put":shrinking}),
        ErrorCode::TransformOverflow,
    );
    rejected(
        &state,
        &global,
        json!({"id":"raw_tracks","put":(0..257).map(|_|json!({"anim":{"target":"t0","track":"opacity","keys":[[0,1]]}})).collect::<Vec<_>>()}),
        ErrorCode::OverBudget(Budget::Tracks),
    );
    let mut coordinates = scene("coordinates");
    let mut budget = GlobalBudget::default();
    put(
        &mut coordinates,
        &mut budget,
        vec![json!({"path":{"id":"at","at":[10000,0],"scale":2,"d":"M0 0 L1 0"}})],
    );
    rejected(
        &coordinates,
        &budget,
        json!({"id":"outside","put":[{"path":{"id":"outside","scale":2,"d":"M0 0 L20000 0"}}]}),
        ErrorCode::OutOfRange,
    );
}

#[test]
fn area_stroke_and_fill_are_reserved_before_preparation() {
    for (points, segments) in [(500, 1065), (0, 64), (1, 64), (2, 69)] {
        let mut state = scene("area");
        let mut budget = GlobalBudget::default();
        put(
            &mut state,
            &mut budget,
            vec![
                json!({"chart":{"id":"c","kind":"area"}}),
                json!({"series":{"id":"s","in":"c","data":vec![1;points]}}),
            ],
        );
        assert_eq!(
            budget.usage().segments,
            segments,
            "area fill, open top-edge stroke and axes are charged for {points} points"
        );
    }
}

#[test]
fn titled_charts_reserve_the_label_origin_inset_at_the_canvas_edge() {
    let state = scene("title");
    let global = GlobalBudget::default();
    for (axis, bounds) in [[19995, 0, 4, 300], [0, 19995, 300, 4]]
        .into_iter()
        .enumerate()
    {
        let mut chart = json!({"id":"c","kind":"line","box":bounds});
        assert!(matches!(
            candidate(
                &state,
                &global,
                json!({"id":"edge","put":[{"chart":chart}]})
            ),
            Ok(Admission::New(_))
        ));
        chart["title"] = json!("T");
        let error = rejected(
            &state,
            &global,
            json!({"id":"edge","put":[{"chart":chart}]}),
            ErrorCode::OutOfRange,
        );
        assert_eq!(error.path.as_str(), format!("put[0].chart.box[{axis}]"));
    }
}

#[test]
fn chart_domains_and_stacked_sums_remain_safe_after_sets_and_detachment() {
    let empty = scene("s");
    let global = GlobalBudget::default();
    for chart in [
        json!({"id":"chart","x":{"min":1,"max":1}}),
        json!({"id":"chart","y":{"scale":"log","min":0}}),
    ] {
        rejected(
            &empty,
            &global,
            json!({"id":"bad","put":[{"chart":chart}]}),
            ErrorCode::OutOfRange,
        );
    }
    rejected(
        &empty,
        &global,
        json!({"id":"sum","put":[
            {"chart":{"id":"c","kind":"area","stacked":true}},
            {"series":{"id":"a","in":"c","data":[1e308]}},{"series":{"id":"b","in":"c","data":[1e308]}}
        ]}),
        ErrorCode::OutOfRange,
    );
    let mut state = scene("log");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"chart":{"id":"c","kind":"line","y":{"scale":"log"}}}),
            json!({"series":{"id":"a","in":"c","data":[1,2]}}),
        ],
    );
    let handle = state.handle(&id("a")).expect("series");
    state
        .stop(&cut(1, vec![handle]), &mut budget)
        .expect("frozen detached series");
    let encoded = serde_json::to_vec(state.recovery_log()).expect("log encodes");
    let log: Vec<SceneEvent> = serde_json::from_slice(&encoded).expect("log decodes");
    let replay = SceneState::replay(header("log"), &log).expect("retained numeric domain replays");
    for (scene, budget) in [(&state, &budget), (&replay, &GlobalBudget::default())] {
        assert!(matches!(
            candidate(
                scene,
                budget,
                json!({"id":"positive","set":[{"target":"a","data":[3]}]})
            ),
            Ok(Admission::New(_))
        ));
        rejected(
            scene,
            budget,
            json!({"id":"negative","set":[{"target":"a","data":[-1]}]}),
            ErrorCode::OutOfRange,
        );
        assert_eq!(scene.handle(&id("a")), Some(handle));
    }
}

#[test]
fn diagram_caps_accept_their_exact_limits_without_charging_failed_additions() {
    let mut state = scene("diagram");
    let mut global = GlobalBudget::default();
    let mut ops = vec![json!({"diagram":{"id":"d"}})];
    ops.extend((0..48).map(|n| json!({"node":{"id":format!("n{n}"),"in":"d","label":""}})));
    ops.extend(
        (0..96).map(|n| json!({"edge":{"id":format!("e{n}"),"in":"d","from":"n0","to":"n1"}})),
    );
    put(&mut state, &mut global, ops);
    assert_eq!(
        (
            state.usage().objects,
            state.usage().segments,
            state.usage().tracks
        ),
        (145, 1824, 0)
    );
    rejected(
        &state,
        &global,
        json!({"id":"node49","put":[{"node":{"id":"extra","in":"d","label":""}}]}),
        ErrorCode::OverBudget(Budget::DiagramNodes),
    );
    rejected(
        &state,
        &global,
        json!({"id":"edge97","put":[{"edge":{"id":"extra","in":"d","from":"n0","to":"n1"}}]}),
        ErrorCode::OverBudget(Budget::DiagramEdges),
    );
}

#[test]
fn a_parent_scale_replacement_keeps_old_descendant_path_charges_until_activation() {
    let mut state = scene("scales");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"diagram":{"id":"g"}}),
            json!({"path":{"id":"p","in":"g","d":"M0 0 Q0 4 4 4"}}),
        ],
    );
    assert_eq!(budget.usage().segments, 4);
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"g","track":"scale","keys":[[0,4]]}})],
    );
    state.retire(1, &mut budget).expect("scale four shown");
    assert_eq!(budget.usage().segments, 8);
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"g","track":"scale","keys":[[0,1]]}})],
    );
    assert_eq!(budget.usage().segments, 8);
    state
        .stop(
            &cut(
                2,
                vec![Handle::new(1, 0).expect("g"), Handle::new(1, 1).expect("p")],
            ),
            &mut budget,
        )
        .expect("unactivated shrink is discarded");
    assert_eq!(budget.usage().segments, 8);
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"g","track":"scale","keys":[[0,1]]}})],
    );
    assert_eq!(budget.usage().segments, 8);
    state.retire(3, &mut budget).expect("shrink shown");
    assert_eq!(budget.usage().segments, 4);
    let replay =
        SceneState::replay(header("scales"), state.recovery_log()).expect("scaled path recovery");
    assert_eq!(replay.usage(), state.usage());
}

#[test]
fn a_detached_replay_has_its_own_mutation_fence_before_budget_installation() {
    let mut source = scene("source");
    let mut original = GlobalBudget::default();
    put(&mut source, &mut original, texts(0, 1));
    let mut detached =
        SceneState::replay(header("source"), source.recovery_log()).expect("inspection replay");
    let mut first = GlobalBudget::default();
    let mut second = GlobalBudget::default();
    let initial = first.generation();
    let Admission::New(pending) =
        candidate(&detached, &first, json!({"id":"new","put":texts(1,1)})).expect("candidate")
    else {
        panic!("new");
    };
    detached
        .stop(&cut(1, vec![]), &mut second)
        .expect("cut binds a different budget");
    assert_eq!(first.generation(), initial);
    assert_eq!(detached.revision(), 1);
    assert_eq!(
        detached.commit(pending, &mut first),
        Err(kumochi_scene::StaleAdmission)
    );
    assert_eq!(
        (first.usage(), first.generation()),
        (Usage::default(), initial)
    );
    assert_eq!((second.usage().objects, second.usage().scenes), (0, 1));
}

#[test]
fn spring_scale_envelopes_and_repeat_counts_remain_positive_and_finite() {
    let mut state = scene("spring");
    let mut budget = GlobalBudget::default();
    put(&mut state, &mut budget, texts(0, 1));
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"anim":{"target":"t0","track":"scale","ease":"spring","keys":[[0,0.1],[1,1]]}}),
        ],
    );
    apply(
        &mut state,
        &mut budget,
        json!({"id":"positive_shrink","put":[{"anim":{"target":"t0","track":"scale","ease":"spring","keys":[[0,1],[1,0.1]]}}]}),
    );
    assert_eq!(
        (state.revision(), budget.usage().tracks, budget.usage().keys),
        (3, 1, 2)
    );
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"t0","track":"scale","repeat":59999,"keys":[[0,1],[1,1]]}})],
    );
    let error = rejected(
        &state,
        &budget,
        json!({"id":"repeat_overflow","put":[{"anim":{"target":"t0","track":"scale","repeat":60000,"keys":[[0,1],[1,1]]}}]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put[0].anim.repeat");
    let mut rotated = scene("rotated");
    let mut global = GlobalBudget::default();
    put(
        &mut rotated,
        &mut global,
        vec![
            json!({"diagram":{"id":"g"}}),
            json!({"text":{"id":"corner","in":"g","at":[20000,20000],"text":""}}),
        ],
    );
    rejected(
        &rotated,
        &global,
        json!({"id":"outside","put":[{"anim":{"target":"g","track":"rotate","keys":[[0,45]]}}]}),
        ErrorCode::OutOfRange,
    );
}

#[test]
fn generated_entrances_do_not_consume_explicit_tracks() {
    let mut state = scene("entrances");
    let mut budget = GlobalBudget::default();
    for chunk in 0..2 {
        put(
            &mut state,
            &mut budget,
            (chunk * 300..chunk * 300 + 300)
                .map(|n| json!({"text":{"id":format!("t{n}"),"at":[0,0],"text":"","enter":"fade"}}))
                .collect(),
        );
    }
    assert_eq!(budget.usage().objects, 600);
    assert_eq!((budget.usage().tracks, budget.usage().keys), (0, 0));
}

#[test]
fn duplicate_owners_and_zero_periods_fail_at_their_source_fields() {
    let mut state = scene("motion_errors");
    let mut budget = GlobalBudget::default();
    put(&mut state, &mut budget, texts(0, 1));
    let error = rejected(
        &state,
        &budget,
        json!({"id":"duplicate","put":[
            {"anim":{"target":"t0","track":"opacity","keys":[[0,0],[100,1]]}},
            {"anim":{"target":"t0","track":"opacity","keys":[[0,1]]}}
        ]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put[1].anim.track");
    for repeat in [-1, 3] {
        let error = rejected(
            &state,
            &budget,
            json!({"id":"zero_repeat","put":[
                {"anim":{"target":"t0","track":"opacity","repeat":repeat,"keys":[[0,1]]}}
            ]}),
            ErrorCode::OutOfRange,
        );
        assert_eq!(error.path.as_str(), "put[0].anim.repeat");
    }
    let error = rejected(
        &state,
        &budget,
        json!({"id":"long_infinite","put":[
            {"anim":{"target":"t0","track":"opacity","repeat":-1,"delay":1,"keys":[[0,0],[60000,1]]}}
        ]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put[0].anim.keys[1][0]");
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"diagram":{"id":"d"}}),
            json!({"node":{"id":"a","in":"d","label":""}}),
            json!({"node":{"id":"b","in":"d","label":""}}),
        ],
    );
    let error = rejected(
        &state,
        &budget,
        json!({"id":"zero_flow","put":[
            {"edge":{"id":"e","in":"d","from":"a","to":"b","flow":0}}
        ]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put[0].edge.flow");
}

#[test]
fn entrance_envelopes_reserve_translation_and_descendant_pivots() {
    for (enter, positive_inside, positive_outside, negative_inside, negative_outside) in [
        ("pop", 17615, 17616, -17616, -17617),
        ("slide", 19985, 19986, -19986, -19987),
        ("fly", 19951, 19952, -19952, -19953),
    ] {
        for (inside, outside) in [
            (positive_inside, positive_outside),
            (negative_inside, negative_outside),
        ] {
            let mut state = scene("entrance_edge");
            let mut budget = GlobalBudget::default();
            put(
                &mut state,
                &mut budget,
                vec![json!({"shape":{
                    "id":"inside","kind":"rect","box":[inside,0,1,1],"enter":enter
                }})],
            );
            rejected(
                &state,
                &budget,
                json!({"id":"outside","put":[{"shape":{
                    "id":"outside","kind":"rect","box":[outside,0,1,1],"enter":enter
                }}]}),
                ErrorCode::OutOfRange,
            );
        }
    }
    let mut state = scene("parent_pivot");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![json!({"diagram":{
            "id":"parent","box":[0,0,10000,10000],"enter":"pop"
        }})],
    );
    put(
        &mut state,
        &mut budget,
        vec![json!({"shape":{
            "id":"inside","in":"parent","kind":"rect","box":[18000,0,1,1]
        }})],
    );
    rejected(
        &state,
        &budget,
        json!({"id":"outside","put":[{"shape":{
            "id":"outside","in":"parent","kind":"rect","box":[18100,0,1,1]
        }}]}),
        ErrorCode::OutOfRange,
    );
}

#[test]
fn detaching_a_scaled_path_preserves_its_future_charge_and_coordinate_basis() {
    let mut state = scene("detached");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"diagram":{"id":"g"}}),
            json!({"path":{"id":"p","in":"g","d":"M0 0 Q0 4 4 4"}}),
        ],
    );
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"g","track":"scale","keys":[[0,4]]}})],
    );
    state.retire(1, &mut budget).expect("scaled parent shown");
    assert_eq!(budget.usage().segments, 8);
    let p = state.handle(&id("p")).expect("path");
    state
        .stop(&cut(2, vec![p]), &mut budget)
        .expect("retain path without parent");
    put(&mut state, &mut budget, texts(0, 1));
    state.retire(2, &mut budget).expect("unrelated step shown");
    assert_eq!(budget.usage().segments, 8);
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"p","track":"scale","keys":[[0,4]]}})],
    );
    assert_eq!(budget.usage().segments, 16);
    let replay =
        SceneState::replay(header("detached"), state.recovery_log()).expect("frozen basis replays");
    assert_eq!(replay.usage(), state.usage());
}

#[test]
fn composed_scale_is_bounded_before_layout_and_rejects_subnormal_products() {
    let mut state = scene("scale_cap");
    let mut budget = GlobalBudget::default();
    put(&mut state, &mut budget, texts(0, 1));
    put(
        &mut state,
        &mut budget,
        vec![json!({"anim":{"target":"t0","track":"scale","keys":[[0,64]]}})],
    );
    let error = rejected(
        &state,
        &budget,
        json!({"id":"too_large","put":[{"anim":{"target":"t0","track":"scale","keys":[[0,65]]}}]}),
        ErrorCode::TransformOverflow,
    );
    assert_eq!(error.path.as_str(), "put");
    let empty = scene("subnormal");
    let error = rejected(
        &empty,
        &GlobalBudget::default(),
        json!({"id":"tiny","put":[
            {"diagram":{"id":"a"}},{"diagram":{"id":"b","in":"a"}},
            {"anim":{"target":"a","track":"scale","keys":[[0,1e-20]]}},
            {"anim":{"target":"b","track":"scale","keys":[[0,1e-20]]}}
        ]}),
        ErrorCode::TransformOverflow,
    );
    assert_eq!(error.path.as_str(), "put");
}

#[test]
fn line_waypoints_and_composed_coordinates_accept_their_exact_boundaries() {
    for (kind, segments) in [("line", 511), ("arrow", 514)] {
        let mut state = scene(kind);
        let mut budget = GlobalBudget::default();
        put(
            &mut state,
            &mut budget,
            vec![json!({kind:{"id":"l","from":[0,0],"to":[1,1],"via":vec![[0,0];510]}})],
        );
        assert_eq!(state.usage().segments, segments);
        let error = rejected(
            &state,
            &budget,
            json!({"id":"excess","put":[{kind:{"id":"extra","from":[0,0],"to":[1,1],"via":vec![[0,0];511]}}]}),
            ErrorCode::OverBudget(Budget::PathPoints),
        );
        assert_eq!(error.path.as_str(), format!("put[0].{kind}.via"));
    }
    let mut state = scene("coordinate");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![json!({"text":{"id":"edge","at":[20000,-20000],"text":""}})],
    );
    let error = rejected(
        &state,
        &budget,
        json!({"id":"outside","put":[{"anim":{"target":"edge","track":"x","keys":[[0,1]]}}]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put");
}

#[test]
fn delay_generated_motion_foreign_admission_and_replay_batch_limits_are_checked() {
    let mut state = scene("reservations");
    let mut budget = GlobalBudget::default();
    put(
        &mut state,
        &mut budget,
        vec![
            json!({"diagram":{"id":"d"}}),
            json!({"node":{"id":"a","in":"d","label":""}}),
            json!({"node":{"id":"b","in":"d","label":""}}),
        ],
    );
    assert_eq!((budget.usage().tracks, budget.usage().keys), (0, 0));
    put(
        &mut state,
        &mut budget,
        vec![json!({"edge":{"id":"e","in":"d","from":"a","to":"b","flow":100}})],
    );
    assert_eq!((budget.usage().tracks, budget.usage().keys), (0, 0));
    assert!(matches!(
        candidate(
            &state,
            &budget,
            json!({"id":"exact","put":[{"anim":{"target":"a","track":"opacity","repeat":1,"keys":[[0,0],[30000,1]]}}]})
        ),
        Ok(Admission::New(_))
    ));
    let error = rejected(
        &state,
        &budget,
        json!({"id":"long","put":[{"anim":{"target":"a","track":"opacity","delay":1,"repeat":1,"keys":[[0,0],[30000,1]]}}]}),
        ErrorCode::OutOfRange,
    );
    assert_eq!(error.path.as_str(), "put[0].anim.repeat");
    let foreign = GlobalBudget::default();
    let error = rejected(
        &state,
        &foreign,
        json!({"id":"foreign","put":texts(1,1)}),
        ErrorCode::BudgetContext,
    );
    assert_eq!(error.path.as_str(), "");
    let mut cleared = scene("cleared");
    let mut isolated = GlobalBudget::default();
    put(&mut cleared, &mut isolated, texts(0, 1));
    cleared
        .stop(&cut(0, vec![]), &mut isolated)
        .expect("never shown");
    let sources = vec![(cleared.header(), cleared.recovery_log()); 5];
    let error =
        SceneState::replay_all(&sources).expect_err("batch length checked even for cleared scenes");
    assert_eq!(error.code, ErrorCode::OverBudget(Budget::VisibleScenes));
    assert_eq!(error.path.as_str(), "");
}

#[test]
fn four_scenes_jointly_fill_the_scene_object_cell_mark_and_segment_caps() {
    let mut budget = GlobalBudget::default();
    let mut scenes: Vec<_> = (0..4).map(|n| scene(&format!("global{n}"))).collect();
    for state in &mut scenes {
        for n in 0..3 {
            put(state, &mut budget, table_ops(&format!("a{n}"), 40));
        }
        put(state, &mut budget, table_ops("last", 5));
        put(state, &mut budget, chart_ops("c0", 8, 500));
        put(state, &mut budget, chart_ops("c1", 2, 500));
        put(
            state,
            &mut budget,
            vec![json!({"path":{"id":"p","d":long_path(6,494)}})],
        );
        put(state, &mut budget, texts(0, 233));
        assert_eq!(
            (
                state.usage().objects,
                state.usage().cells,
                state.usage().marks,
                state.usage().segments
            ),
            (375, 1000, 5000, 5000)
        );
    }
    let usage = budget.usage();
    assert_eq!(
        (
            usage.scenes,
            usage.objects,
            usage.cells,
            usage.marks,
            usage.segments
        ),
        (4, 1500, 4000, 20000, 20000)
    );
    rejected(
        &scenes[0],
        &budget,
        json!({"id":"one_more_object","put":texts(233,1)}),
        ErrorCode::OverBudget(Budget::LiveObjects),
    );
    apply(
        &mut scenes[0],
        &mut budget,
        json!({"id":"free_slot","remove":["t0"]}),
    );
    let index = scenes[0].revision() - 1;
    scenes[0]
        .retire(index, &mut budget)
        .expect("one slot released");
    for (op, code) in [
        (
            json!({"row":{"id":"extra","in":"lasttable","cells":[]}}),
            Budget::TableCells,
        ),
        (
            json!({"series":{"id":"extra","in":"c1chart","data":[1]}}),
            Budget::ChartMarks,
        ),
        (
            json!({"path":{"id":"extra","d":"M0 0 L1 0"}}),
            Budget::PathSegments,
        ),
    ] {
        rejected(
            &scenes[0],
            &budget,
            json!({"id":"probe","put":[op]}),
            ErrorCode::OverBudget(code),
        );
    }
    let sources: Vec<_> = scenes
        .iter()
        .map(|state| (state.header(), state.recovery_log()))
        .collect();
    let (restored, installed) =
        SceneState::replay_all(&sources).expect("four cap-filled scenes recover");
    assert_eq!(installed.usage(), budget.usage());
    assert_eq!(restored.len(), 4);
}

/// Dashed straight paths whose admission charges, floor(length / 10) + 2 per edge + 1, sum to
/// exactly `charge`; each carries 3 to 4,003.
fn dashed_filler(charge: usize) -> Vec<Value> {
    const FULL: usize = 4_003;
    let mut charges = vec![FULL; charge / FULL];
    match charge % FULL {
        0 => {}
        rest @ (1 | 2) => {
            charges
                .pop()
                .expect("a charge past 4,003 has a full path to split");
            charges.extend([2_000, FULL + rest - 2_000]);
        }
        rest => charges.push(rest),
    }
    charges
        .into_iter()
        .enumerate()
        .map(|(n, charge)| {
            let end = 10 * i32::try_from(charge - 3).expect("bounded fixture") - 20_000;
            json!({"path":{"id":format!("fill{n}"),"d":format!("M-20000 0 L{end} 0"),"dash":true}})
        })
        .collect()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one boundary table covers every explicit dashed producer"
)]
fn explicit_dashed_geometry_is_charged_before_layout_at_the_limit_and_one_over() {
    use kumochi_scene::limits::MAX_GLOBAL_DASH_PIECES as MAX;
    let zigzag: Vec<[f32; 2]> = (0..10_u8)
        .map(|n| [f32::from(n), if n % 2 == 0 { 0.0 } else { 100.0 }])
        .collect();
    // Charges worked by hand: floor(length / 10) + 2 · segments + 1.
    for (name, op, charge) in [
        (
            "path",
            json!({"path":{"id":"x","d":"M0 0 L100 0","dash":true}}),
            13,
        ),
        (
            "closed path",
            json!({"path":{"id":"x","d":"M0 0 L100 0 L100 100 Z","dash":true}}),
            41,
        ),
        (
            "curve",
            json!({"path":{"id":"x","d":"M0 0 Q50 50 100 0","dash":true}}),
            47,
        ),
        (
            "scaled path",
            json!({"path":{"id":"x","d":"M0 0 L100 0","scale":2,"dash":true}}),
            23,
        ),
        (
            "line",
            json!({"line":{"id":"x","from":[0,0],"to":[100,0],"dash":true}}),
            13,
        ),
        (
            "line via",
            json!({"line":{"id":"x","from":[0,0],"via":[[100,0]],"to":[100,100],"dash":true}}),
            25,
        ),
        (
            "arrow",
            json!({"arrow":{"id":"x","from":[0,0],"to":[100,0],"dash":true}}),
            19,
        ),
        (
            "bent arrow",
            json!({"arrow":{"id":"x","from":[0,0],"to":[52,0],"bend":0.75,"dash":true}}),
            45,
        ),
        (
            "rect",
            json!({"shape":{"id":"x","kind":"rect","box":[0,0,100,60],"radius":10,"dash":true}}),
            91,
        ),
        (
            "ellipse",
            json!({"shape":{"id":"x","kind":"ellipse","box":[0,0,100,60],"dash":true}}),
            129,
        ),
        (
            "circle",
            json!({"shape":{"id":"x","kind":"circle","at":[0,0],"r":50,"dash":true}}),
            137,
        ),
        // Its own closed sequence, 1,000.45 long, not its 9 × 100 box: a box charge is 42.
        (
            "zigzag polygon",
            json!({"shape":{"id":"x","kind":"polygon","points":zigzag,"dash":true}}),
            121,
        ),
    ] {
        let mut state = scene("dashes");
        let mut global = GlobalBudget::default();
        let mut over = dashed_filler(MAX - charge + 1);
        over.push(op.clone());
        rejected(
            &state,
            &global,
            json!({"id":"over","put":over}),
            ErrorCode::OverBudget(Budget::DashPieces),
        );
        let mut at = dashed_filler(MAX - charge);
        at.push(op);
        put(&mut state, &mut global, at);
        assert_eq!(
            global.usage().dash_pieces,
            MAX,
            "{name} reaches the cap exactly"
        );
        rejected(
            &state,
            &global,
            json!({"id":"more","put":[{"line":{"id":"more","from":[0,0],"to":[1,0],"dash":true}}]}),
            ErrorCode::OverBudget(Budget::DashPieces),
        );
        put(
            &mut state,
            &mut global,
            vec![json!({"line":{"id":"solid","from":[0,0],"to":[1,0]}})],
        );
        assert_eq!(
            global.usage().dash_pieces,
            MAX,
            "{name}: a refusal charged nothing"
        );
    }
}

#[test]
fn font_dependent_dashes_charge_nothing_until_layout() {
    use kumochi_scene::limits::MAX_GLOBAL_DASH_PIECES as MAX;
    let mut state = scene("refs");
    let mut global = GlobalBudget::default();
    let mut ops = dashed_filler(MAX);
    ops.extend([
        json!({"shape":{"id":"a","kind":"circle","at":[0,0],"r":10}}),
        json!({"shape":{"id":"b","kind":"circle","at":[10000,0],"r":10}}),
        json!({"line":{"id":"ref","from":{"ref":"a"},"to":{"ref":"b"},"dash":true}}),
        json!({"diagram":{"id":"d","box":[0,0,400,300]}}),
        json!({"node":{"id":"n0","in":"d","label":"A"}}),
        json!({"node":{"id":"n1","in":"d","label":"B"}}),
        json!({"edge":{"id":"e","in":"d","from":"n0","to":"n1","dash":true}}),
    ]);
    put(&mut state, &mut global, ops);
    assert_eq!(global.usage().dash_pieces, MAX);
}

#[test]
fn a_dashed_op_without_edges_charges_nothing() {
    let mut state = scene("moves");
    let mut global = GlobalBudget::default();
    put(
        &mut state,
        &mut global,
        vec![json!({"path":{"id":"moves","d":"M0 0 M5 5","dash":true}})],
    );
    assert_eq!(global.usage().dash_pieces, 0);
}
