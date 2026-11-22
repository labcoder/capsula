//! The presenter's binding contract: detached admission, atomic apply and exact release.
use kumochi_scene::{
    Admission, BindingError, Budget, Committed, ErrorCode, GlobalBudget, Handle, Preparation,
    Record, SceneBinding, SceneState, Step, StopCut, StopOutcome, WireId,
    budget::Usage,
    deferred::Deferred,
    display::WideMeasure,
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    limits::{
        MAX_GLOBAL_CHART_MARKS, MAX_GLOBAL_LIVE_OBJECTS, MAX_GLOBAL_PATH_SEGMENTS,
        MAX_GLOBAL_TABLE_CELLS, MAX_VISIBLE_SCENES,
    },
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

fn header() -> kumochi_scene::Begin {
    let source = br#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":""}}"#;
    let Ok(Record::Begin(header)) = parse_record(source) else {
        panic!("begin fixture");
    };
    header
}
fn step(json: &str) -> Step {
    let record = format!(r#"{{"step":{json}}}"#);
    let Ok(Record::Step(step)) = parse_record(record.as_bytes()) else {
        panic!("step fixture");
    };
    step
}
fn text(name: &str, object: &str) -> String {
    format!(r#"{{"id":"{name}","put":[{{"text":{{"id":"{object}","text":"","at":[0,0]}}}}]}}"#)
}
fn id(value: &str) -> WireId {
    value.to_owned().try_into().expect("valid id")
}
fn commit(state: &mut SceneState, global: &mut GlobalBudget, json: &str) -> Committed {
    let Admission::New(admitted) = state.admit(&step(json), global).expect("valid step") else {
        panic!("new step");
    };
    state.commit(admitted, global).expect("fresh admission")
}
fn bound(global: &mut GlobalBudget) -> SceneBinding {
    let state = SceneState::begin(header()).expect("valid header");
    SceneBinding::bind(Deferred::new(state, 0), global).expect("an empty scene binds")
}
fn apply(
    binding: &mut SceneBinding,
    global: &mut GlobalBudget,
    json: &str,
) -> Result<Preparation, BindingError> {
    let read = binding.read();
    let candidate = read.admit_detached(&step(json)).expect("valid step");
    binding.apply_detached(read, candidate, global)
}
fn cut(activated: u32) -> StopCut {
    let first = Handle::new(1, 0).expect("the first step's object");
    StopCut {
        activated,
        revealed: Bounded::new(vec![first]).expect("one handle"),
    }
}
/// Everything a rejected operation must leave alone.
fn snapshot(
    binding: &SceneBinding,
    global: &GlobalBudget,
) -> (u32, usize, usize, Usage, Usage, u64) {
    let view = binding.view();
    (
        view.revision(),
        view.object_count(),
        view.recovery_log().len(),
        view.usage(),
        global.usage(),
        global.generation(),
    )
}

#[test]
fn bind_charges_an_unbound_scene_once_and_adopts_a_batch_bound_one() {
    let mut brain = GlobalBudget::default();
    let mut source = SceneState::begin(header()).expect("valid header");
    commit(&mut source, &mut brain, &text("a", "t1"));
    let replayed = || SceneState::replay(header(), source.recovery_log()).expect("replay");
    let usage = Usage {
        objects: 1,
        scenes: 1,
        ..Usage::default()
    };

    let mut global = GlobalBudget::default();
    let initial = global.generation();
    let empty = bound(&mut global);
    assert_eq!(global.usage(), Usage::default());
    assert_eq!(global.generation(), initial);
    assert_eq!(empty.view().revision(), 0);
    let generation = global.generation();
    let binding = SceneBinding::bind(Deferred::new(replayed(), 0), &mut global).expect("charged");
    assert_eq!(global.usage(), usage, "an unbound scene is charged once");
    assert_eq!(
        binding.view().usage(),
        usage,
        "the binding owns that charge"
    );
    assert_ne!(global.generation(), generation, "the prior stamp is stale");

    let mut full = GlobalBudget::default();
    let held: Vec<_> = (0..MAX_VISIBLE_SCENES)
        .map(|_| SceneBinding::bind(Deferred::new(replayed(), 0), &mut full).expect("at the cap"))
        .collect();
    let before = (full.usage(), full.generation());
    let over = SceneBinding::bind(Deferred::new(replayed(), 0), &mut full).expect_err("one over");
    assert_eq!(
        over,
        BindingError::OverBudget(ErrorCode::OverBudget(Budget::VisibleScenes)),
        "a fifth visible scene is refused"
    );
    assert_eq!((full.usage(), full.generation()), before, "nothing charged");
    drop(held);

    let before = (global.usage(), global.generation());
    let foreign = SceneBinding::bind(Deferred::new(source, 0), &mut global).expect_err("foreign");
    assert_eq!(
        foreign,
        BindingError::WrongContext,
        "bound to the brain's budget"
    );
    assert_eq!(
        (global.usage(), global.generation()),
        before,
        "nothing charged"
    );

    let begin = header();
    let (mut scenes, mut batch) =
        SceneState::replay_all(&[(&begin, binding.view().recovery_log())]).expect("batch");
    let before = (batch.usage(), batch.generation());
    let adopted = SceneBinding::bind(Deferred::new(scenes.remove(0), 0), &mut batch)
        .expect("already charged to this budget");
    assert_eq!(
        (batch.usage(), batch.generation()),
        before,
        "adopted without a second charge"
    );
    assert_eq!(
        batch.usage(),
        adopted.view().usage(),
        "the binding owns the batch charge"
    );
}

#[test]
fn a_detached_candidate_installs_and_prepares_its_object() {
    let mut global = GlobalBudget::default();
    let mut binding = bound(&mut global);
    let before = LayoutState::new(binding.view());
    let preparation = apply(&mut binding, &mut global, &text("a", "t1")).expect("applied");
    let handle = Handle::new(1, 0).expect("first handle");
    let usage = Usage {
        objects: 1,
        scenes: 1,
        ..Usage::default()
    };
    assert_eq!(binding.view().revision(), 1);
    assert_eq!(binding.view().usage(), usage);
    assert_eq!(global.usage(), usage);
    assert_eq!(binding.view().handle(&id("t1")), Some(handle));

    let input = PrepareInput::new(
        preparation,
        before.clone(),
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    )
    .expect("joins the bound scene's original layout");
    let prepared = prepare(input, &mut WideMeasure, &AtomicBool::new(false))
        .expect("prepares the detached commit");
    prepared
        .validate(binding.view(), &before)
        .expect("current result");
    assert_eq!(prepared.after.handle(&id("t1")), Some(handle));
    assert_eq!(
        prepared
            .after
            .items()
            .into_iter()
            .map(|item| item.handle)
            .collect::<Vec<_>>(),
        [handle]
    );

    let read = binding.read();
    let retransmitted = read
        .admit_detached(&step(&text("a", "t1")))
        .expect_err("known id");
    assert_eq!(
        retransmitted.code,
        ErrorCode::DuplicateStepConflict,
        "a mirror never accepts a retransmission"
    );
    let reused = read
        .admit_detached(&step(&text("b", "t1")))
        .expect_err("reserved");
    assert_eq!(
        reused.code,
        ErrorCode::IdReused,
        "the created id stays reserved"
    );
}

#[test]
fn stale_foreign_or_shared_candidates_change_nothing() {
    let mut global = GlobalBudget::default();
    let mut scene = bound(&mut global);
    let other = bound(&mut global);
    let handles =
        |scene: &SceneBinding| ["t1", "t2", "t3"].map(|name| scene.view().handle(&id(name)));
    apply(&mut scene, &mut global, &text("a", "t1")).expect("first step");
    let next = step(&text("b", "t2"));
    let before = snapshot(&scene, &global);
    let first = scene.view().handle(&id("t1")).expect("first object");

    let foreign = other.read();
    let candidate = foreign.admit_detached(&next).expect("valid step");
    assert_eq!(
        scene
            .apply_detached(foreign, candidate, &mut global)
            .expect_err("another scene's read"),
        BindingError::WrongOwner
    );
    let foreign = other.read();
    let candidate = foreign.admit_detached(&next).expect("valid step");
    drop(foreign);
    assert_eq!(
        scene
            .apply_detached(scene.read(), candidate, &mut global)
            .expect_err("another scene's candidate"),
        BindingError::WrongOwner
    );

    let mut replaced = GlobalBudget::default();
    let budget = (replaced.usage(), replaced.generation());
    let read = scene.read();
    let candidate = read.admit_detached(&next).expect("valid step");
    assert_eq!(
        scene
            .apply_detached(read, candidate, &mut replaced)
            .expect_err("a replaced budget"),
        BindingError::WrongContext
    );
    assert_eq!(
        (replaced.usage(), replaced.generation()),
        budget,
        "the new budget is untouched"
    );

    let read = scene.read();
    let shared = scene.read();
    let candidate = read.admit_detached(&next).expect("valid step");
    let error = scene.apply_detached(read, candidate, &mut global);
    assert_eq!(
        error.expect_err("shared"),
        BindingError::NotUnique,
        "no clone fallback"
    );
    drop(shared);
    assert_eq!(
        snapshot(&scene, &global),
        before,
        "every rejection changed nothing"
    );
    assert_eq!(handles(&scene), [Some(first), None, None]);

    let read = scene.read();
    let stale = read.admit_detached(&next).expect("valid step");
    drop(read);
    apply(&mut scene, &mut global, &text("c", "t3")).expect("an intervening commit");
    let third = scene.view().handle(&id("t3")).expect("intervening object");
    let before = snapshot(&scene, &global);
    let error = scene.apply_detached(scene.read(), stale, &mut global);
    assert_eq!(
        error.expect_err("stale"),
        BindingError::WrongRevision,
        "an older revision"
    );
    assert_eq!(
        snapshot(&scene, &global),
        before,
        "a stale revision changed nothing"
    );
    assert_eq!(handles(&scene), [Some(first), None, Some(third)]);

    let read = scene.read();
    let stale = read.admit_detached(&next).expect("valid step");
    drop(read);
    scene.retire(0, &mut global).expect("a kept step");
    let before = snapshot(&scene, &global);
    let error = scene.apply_detached(scene.read(), stale, &mut global);
    assert_eq!(
        error.expect_err("stale"),
        BindingError::WrongMutation,
        "same revision, newer state"
    );
    assert_eq!(
        snapshot(&scene, &global),
        before,
        "stale candidates changed nothing"
    );
    assert_eq!(handles(&scene), [Some(first), None, Some(third)]);

    apply(&mut scene, &mut global, &text("b", "t2")).expect("a fresh candidate still applies");
    assert_eq!(scene.view().revision(), 3);
    assert_eq!(
        scene.view().handle(&id("t2")),
        Some(Handle::new(3, 0).expect("retry's handle"))
    );
}

#[test]
fn apply_checks_the_current_budget_at_the_cap_and_one_over() {
    let mut global = GlobalBudget::default();
    let mut scenes: Vec<_> = (0..=MAX_VISIBLE_SCENES)
        .map(|_| bound(&mut global))
        .collect();
    let mut pending: Vec<_> = scenes
        .iter()
        .map(|scene| {
            let read = scene.read();
            let candidate = read
                .admit_detached(&step(&text("a", "t1")))
                .expect("valid step");
            (read, candidate)
        })
        .collect();
    let (read, candidate) = pending.pop().expect("one candidate per scene");
    for (scene, (read, candidate)) in scenes.iter_mut().zip(pending) {
        scene
            .apply_detached(read, candidate, &mut global)
            .expect("within the visible-scene cap");
    }
    assert_eq!(
        global.usage().scenes,
        MAX_VISIBLE_SCENES,
        "exactly at the cap"
    );
    let last = scenes.last_mut().expect("one more scene");
    let before = snapshot(last, &global);
    let error = last.apply_detached(read, candidate, &mut global);
    assert_eq!(
        error.expect_err("one over"),
        BindingError::OverBudget(ErrorCode::OverBudget(Budget::VisibleScenes)),
        "checked against the budget at apply, not at admission"
    );
    assert_eq!(
        snapshot(last, &global),
        before,
        "a refused apply changed nothing"
    );

    scenes
        .remove(0)
        .release(&mut global)
        .expect("its own budget");
    let last = scenes.last_mut().expect("one more scene");
    apply(last, &mut global, &text("a", "t1")).expect("the released capacity admits it");
    assert_eq!(global.usage().scenes, MAX_VISIBLE_SCENES, "back at the cap");
}

#[test]
fn release_subtracts_exactly_its_charge_and_a_foreign_budget_changes_nothing() {
    let mut global = GlobalBudget::default();
    let mut kept = bound(&mut global);
    let mut released = bound(&mut global);
    apply(&mut kept, &mut global, &text("a", "t1")).expect("one object");
    let two = r#"{"id":"b","put":[{"text":{"id":"u1","text":"","at":[0,0]}},{"text":{"id":"u2","text":"","at":[0,0]}}]}"#;
    apply(&mut released, &mut global, two).expect("two objects");
    assert_eq!(global.usage().objects, 3, "both scenes charged");
    let before_release = global.generation();
    released.release(&mut global).expect("its own budget");
    assert_eq!(
        global.usage(),
        Usage {
            objects: 1,
            scenes: 1,
            ..Usage::default()
        },
        "only the released charge left"
    );
    assert_ne!(
        global.generation(),
        before_release,
        "the released charge changes the stamp"
    );
    assert_eq!(
        kept.view().handle(&id("t1")),
        Some(Handle::new(1, 0).expect("retained handle"))
    );

    let mut foreign = GlobalBudget::default();
    let before = (foreign.usage(), foreign.generation());
    let error = kept.release(&mut foreign).expect_err("foreign");
    assert_eq!(
        error,
        BindingError::WrongContext,
        "another budget's binding"
    );
    assert_eq!(
        (foreign.usage(), foreign.generation()),
        before,
        "nothing subtracted"
    );
}

#[test]
fn stop_and_retire_mutate_only_a_unique_binding_through_its_own_budget() {
    let mut global = GlobalBudget::default();
    let mut scene = bound(&mut global);
    apply(&mut scene, &mut global, &text("a", "t1")).expect("first step");
    apply(&mut scene, &mut global, &text("b", "t2")).expect("second step");
    let first = scene.view().handle(&id("t1")).expect("first object");
    let before = snapshot(&scene, &global);

    let read = scene.read();
    let error = scene.retire(0, &mut global).expect_err("shared");
    assert_eq!(
        error,
        BindingError::NotUnique,
        "retire waits for the snapshot"
    );
    let error = scene.stop(&cut(1), &mut global).expect_err("shared");
    assert_eq!(
        error,
        BindingError::NotUnique,
        "stop waits for the snapshot"
    );
    drop(read);
    let mut foreign = GlobalBudget::default();
    let error = scene.retire(0, &mut foreign).expect_err("foreign");
    assert_eq!(
        error,
        BindingError::WrongContext,
        "retire through another budget"
    );
    let error = scene.stop(&cut(1), &mut foreign).expect_err("foreign");
    assert_eq!(
        error,
        BindingError::WrongContext,
        "stop through another budget"
    );
    let Err(BindingError::Rejected(error)) = scene.retire(5, &mut global) else {
        panic!("an index never committed is rejected");
    };
    assert_eq!(error.code, ErrorCode::OutOfRange, "the scene's own error");
    assert_eq!(
        snapshot(&scene, &global),
        before,
        "every rejection changed nothing"
    );

    scene
        .retire(0, &mut global)
        .expect("the zero-based first step");
    assert_eq!(scene.view().activated(), 1, "retired through revision one");
    assert_eq!(
        global.usage(),
        scene.view().usage(),
        "charged through the real budget"
    );
    let outcome = scene.stop(&cut(1), &mut global).expect("a kept cut");
    assert_eq!(outcome, StopOutcome::Kept, "the first step stays");
    assert_eq!(
        scene.view().object_count(),
        1,
        "the unactivated object is dropped"
    );
    assert_eq!(scene.view().handle(&id("t1")), Some(first));
    assert_eq!(scene.view().handle(&id("t2")), None);
    assert_eq!(global.usage(), scene.view().usage(), "the cut is charged");
}

fn put(id: &str, ops: &[Value]) -> String {
    json!({ "id": id, "put": ops }).to_string()
}
fn texts(count: usize) -> Vec<Value> {
    (0..count)
        .map(|n| json!({"text":{"id":format!("t{n}"),"at":[0,0],"text":""}}))
        .collect()
}
fn table(prefix: &str, rows: usize) -> Vec<Value> {
    let mut ops =
        vec![json!({"table":{"id":format!("{prefix}table"),"cols":["","","","","","","",""]}})];
    ops.extend((0..rows).map(
        |n| json!({"row":{"id":format!("{prefix}r{n}"),"in":format!("{prefix}table"),"cells":[]}}),
    ));
    ops
}
fn chart(prefix: &str, series: usize, points: usize) -> Vec<Value> {
    let mut ops = vec![json!({"chart":{"id":format!("{prefix}chart"),"kind":"scatter"}})];
    ops.extend((0..series).map(|n| {
        json!({"series":{"id":format!("{prefix}s{n}"),"in":format!("{prefix}chart"),"data":vec![1; points]}})
    }));
    ops
}
fn path(id: &str, curves: usize, lines: usize) -> Vec<Value> {
    let d = format!(
        "{}{}",
        "M0 0 C0 20000 20000 20000 20000 0 ".repeat(curves),
        "L0 0 ".repeat(lines)
    );
    vec![json!({"path":{"id":id,"d":d}})]
}

/// Fills bound scenes exactly to one global cap, then has the worker accept a step one over it
/// that only apply refuses. A fitting candidate from the same snapshot still installs after the
/// refusal, so the revision and mutation stamps stayed put.
fn refused_at_cap(
    scenes: usize,
    fill: Vec<(usize, Vec<Value>)>,
    over: (usize, Vec<Value>),
    budget: Budget,
    used: fn(Usage) -> usize,
    cap: usize,
) {
    let mut global = GlobalBudget::default();
    let mut bindings: Vec<_> = (0..scenes).map(|_| bound(&mut global)).collect();
    for (n, (scene, ops)) in fill.into_iter().enumerate() {
        apply(
            &mut bindings[scene],
            &mut global,
            &put(&format!("fill{n}"), &ops),
        )
        .expect("within the cap");
    }
    assert_eq!(used(global.usage()), cap, "{budget:?} exactly at the cap");
    let (scene, ops) = over;
    let target = &mut bindings[scene];
    let read = target.read();
    let candidate = read
        .admit_detached(&step(&put("over", &ops)))
        .expect("the worker checks no global cap");
    let fits = read
        .admit_detached(&step(r#"{"id":"empty"}"#))
        .expect("an empty step");
    let before = snapshot(target, &global);
    let error = target.apply_detached(read, candidate, &mut global);
    assert_eq!(
        error.expect_err("one over"),
        BindingError::OverBudget(ErrorCode::OverBudget(budget)),
        "{budget:?} refused at apply"
    );
    assert_eq!(
        snapshot(target, &global),
        before,
        "{budget:?} refusal changed nothing"
    );
    let read = target.read();
    target
        .apply_detached(read, fits, &mut global)
        .expect("the snapshot's stamps are still current");
    assert_eq!(
        used(global.usage()),
        cap,
        "{budget:?} still exactly at the cap"
    );
}

#[test]
fn global_objects_cells_marks_and_segments_are_checked_at_apply_at_the_cap_and_one_over() {
    let extra = vec![json!({"text":{"id":"extra","at":[0,0],"text":""}})];
    refused_at_cap(
        3,
        (0..3).map(|scene| (scene, texts(500))).collect(),
        (2, extra),
        Budget::LiveObjects,
        |usage| usage.objects,
        MAX_GLOBAL_LIVE_OBJECTS,
    );
    let mut tables: Vec<_> = (0..12).map(|n| (0, table(&format!("a{n}"), 40))).collect();
    tables.push((0, table("last", 20)));
    tables.push((0, vec![json!({"table":{"id":"narrow","cols":[""]}})]));
    let row = vec![json!({"row":{"id":"extra","in":"narrow","cells":[]}})];
    refused_at_cap(
        1,
        tables,
        (0, row),
        Budget::TableCells,
        |usage| usage.cells,
        MAX_GLOBAL_TABLE_CELLS,
    );
    refused_at_cap(
        1,
        (0..5)
            .map(|n| (0, chart(&format!("a{n}"), 8, 500)))
            .collect(),
        (0, chart("extra", 1, 1)),
        Budget::ChartMarks,
        |usage| usage.marks,
        MAX_GLOBAL_CHART_MARKS,
    );
    let mut paths: Vec<_> = (0..4).map(|n| (0, path(&format!("p{n}"), 8, 0))).collect();
    paths.push((0, path("last", 7, 32)));
    let line = vec![json!({"path":{"id":"extra","d":"M0 0 L1 0"}})];
    refused_at_cap(
        1,
        paths,
        (0, line),
        Budget::PathSegments,
        |usage| usage.segments,
        MAX_GLOBAL_PATH_SEGMENTS,
    );
}
