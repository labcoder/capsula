use std::{error::Error as _, sync::Arc};

use kumochi_protocol::message::{RunId, SceneHeader, SceneId, ThreadId};
use kumochi_scene::{
    Admission, GlobalBudget, Handle, Op, SceneEvent, SceneState, Step, StopCut, WireId,
    record::Point,
    text::{Bounded, Number},
};

use super::{MAX_BODY_BYTES, Store, StoreError};
use crate::{
    drawing::{Reply, Target, Write},
    test_support::Scratch,
};

fn begin(store: &mut Store) -> (ThreadId, RunId) {
    let (thread, run) = store.high_water().expect("high water");
    let ids = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(ids.0, Some("title"), ids.1, "turn")
        .expect("turn");
    ids
}

#[test]
#[ignore = "manual quiet-Rez release measurement, not a CI latency assertion"]
fn measure_dense_scene_append_at_lifetime_cap() {
    use std::time::Instant;
    let mut put = vec![serde_json::json!({"chart":{"id":"c","kind":"line","box":[0,0,100,100]}})];
    let mut patches = Vec::new();
    for series in 0..8 {
        let data = vec![51.248_178_375_505_404; 200];
        put.push(serde_json::json!({"series":{"id":format!("s{series}"),"in":"c","data":data}}));
        patches.push(serde_json::json!({"target":format!("s{series}"),"data":data}));
    }
    let first_bytes =
        serde_json::to_vec(&serde_json::json!({"step":{"id":"dense","put":put}})).expect("JSON");
    assert_eq!(first_bytes.len(), 30_814);
    let kumochi_scene::Record::Step(first_step) =
        kumochi_scene::parse_record(&first_bytes).expect("legal record")
    else {
        panic!("step")
    };
    let first_step = Arc::new(first_step);
    let updates: Vec<_> = (2..=64)
        .map(|revision| {
            let bytes = serde_json::to_vec(
                &serde_json::json!({"step":{"id":format!("update{revision}"),"set":patches}}),
            )
            .expect("JSON");
            let kumochi_scene::Record::Step(step) =
                kumochi_scene::parse_record(&bytes).expect("legal update")
            else {
                panic!("step")
            };
            Arc::new(step)
        })
        .collect();
    let mut samples = Vec::new();
    for sample in 0..16 {
        let scratch = Scratch::new("dense-append-cap");
        let mut store = Store::open(&scratch.db()).expect("store");
        let ids = begin(&mut store);
        let start = Instant::now();
        let Reply::Step(scene) = write(
            &mut store,
            ids,
            Target::New(header("dense")),
            1,
            Arc::clone(&first_step),
        )
        .expect("first step") else {
            panic!("step reply")
        };
        let first_ns = start.elapsed().as_nanos();
        for (revision, step) in (2..64).zip(&updates) {
            write(
                &mut store,
                ids,
                Target::Existing(scene),
                revision,
                Arc::clone(step),
            )
            .expect("prefix step");
        }
        let start = Instant::now();
        write(
            &mut store,
            ids,
            Target::Existing(scene),
            64,
            Arc::clone(updates.last().expect("last step")),
        )
        .expect("cap step");
        let cap_ns = start.elapsed().as_nanos();
        let stored = store.drawing_scenes().expect("full log");
        assert_eq!(stored[0].log.len(), 64);
        let replayed = SceneState::replay(stored[0].header.begin.clone(), &stored[0].log)
            .expect("valid canonical scene");
        assert_eq!(replayed.revision(), 64);
        if sample >= 8 {
            samples.push((sample - 8, first_ns, cap_ns));
        }
    }
    println!(
        "dense_scene_append points=1600 lifetime_steps=64 warmups=8 samples=8 includes_transaction=true"
    );
    for (sample, first_ns, cap_ns) in samples {
        println!("sample,{sample},{first_ns},{cap_ns}");
    }
}

fn header(name: &str) -> SceneHeader {
    serde_json::from_value(serde_json::json!({
        "begin": {"v":1,"scene":name,"surface":"overlay","alt":"privatealt"},
        "layout_version":1
    }))
    .expect("header")
}

fn step(id: &str) -> Arc<Step> {
    Arc::new(
        serde_json::from_value(serde_json::json!({
            "id":id,"say":"privatenarration",
            "put":[{"shape":{"id":format!("object-{id}"),"kind":"rect","box":[0,0,10,10]}}]
        }))
        .expect("step"),
    )
}

fn write(
    store: &mut Store,
    ids: (ThreadId, RunId),
    target: Target,
    revision: u32,
    step: Arc<Step>,
) -> Result<Reply, StoreError> {
    store.drawing_write(&Write::Step {
        thread: ids.0,
        run: ids.1,
        target,
        revision,
        layout_version: 1,
        step,
    })
}

fn first(store: &mut Store, ids: (ThreadId, RunId), name: &str) -> SceneId {
    let Reply::Step(scene) =
        write(store, ids, Target::New(header(name)), 1, step("a")).expect("first")
    else {
        panic!("wrong reply");
    };
    scene
}

fn cut(
    store: &mut Store,
    scene: SceneId,
    revision: u32,
    activated: u32,
) -> Result<Reply, StoreError> {
    store.drawing_write(&Write::Cut {
        scene,
        revision,
        cut: StopCut {
            activated,
            revealed: Bounded::default(),
        },
    })
}

fn count(store: &Store, kind: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM events WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )
        .expect("count")
}

fn corrupt_step(store: &Store, body: &str) {
    store
        .conn
        .execute_batch("DROP TRIGGER events_append_only")
        .expect("allow fixture mutation");
    store
        .conn
        .execute(
            "UPDATE events SET body = ?1 WHERE kind = 'scene_step'",
            [body],
        )
        .expect("corrupt");
}

fn drawing_error<T>(result: Result<T, StoreError>) {
    let Err(error) = result else {
        panic!("expected drawing failure")
    };
    assert!(matches!(error, StoreError::Drawing));
    assert_eq!(error.to_string(), "can't store or recover the drawing");
    assert_eq!(format!("{error:?}"), "Drawing");
    assert!(error.source().is_none());
}

#[test]
fn first_step_failure_rolls_back_header_and_hides_sql_error_content() {
    let scratch = Scratch::new("typed-rollback");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    store.conn.execute_batch("CREATE TRIGGER fail_drawing BEFORE INSERT ON events WHEN NEW.kind = 'scene_step' BEGIN SELECT RAISE(ABORT, 'privatecontent'); END;").expect("trigger");
    drawing_error(write(
        &mut store,
        ids,
        Target::New(header("private-scene")),
        1,
        step("a"),
    ));
    assert_eq!(count(&store, "scene_begin"), 0);
    assert_eq!(count(&store, "scene_step"), 0);
    assert_eq!(store.drawing_scenes().expect("empty"), Vec::new());
    store
        .conn
        .execute_batch("DROP TRIGGER fail_drawing")
        .expect("untrap");
    first(&mut store, ids, "private-scene");
    assert_eq!(count(&store, "scene_begin"), 1);
}

#[test]
fn drawing_content_never_reaches_search_or_page_text() {
    let scratch = Scratch::new("typed-private");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "privatename");
    cut(&mut store, scene, 1, 1).expect("cut");
    let texts: i64 = store.conn.query_row("SELECT count(*) FROM events WHERE kind IN ('scene_begin','scene_step','scene_cut') AND text IS NOT NULL", [], |row| row.get(0)).expect("texts");
    assert_eq!(texts, 0);
    let hits: i64 = store.conn.query_row("SELECT count(*) FROM events_fts WHERE events_fts MATCH 'privatenarration OR privatealt OR privatename'", [], |row| row.get(0)).expect("fts");
    assert_eq!(hits, 0);
    let (page, _) = store
        .pages()
        .events_page(ids.0, None, 20)
        .expect("page accepts cut metadata");
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].text, "");
    assert_eq!(page[1].text, "turn");
}

#[test]
fn cleared_names_survive_reopen_but_are_not_recovered_or_reused() {
    let scratch = Scratch::new("typed-tombstone");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "used");
    store.clear_scenes(&[scene]).expect("clear");
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store
            .drawing_write(&Write::Lookup {
                thread: ids.0,
                name: WireId::try_from("used".to_owned()).expect("name")
            })
            .expect("lookup"),
        Reply::Lookup(Some(scene))
    );
    assert!(store.drawing_scenes().expect("recovery").is_empty());
    drawing_error(write(
        &mut store,
        ids,
        Target::New(header("used")),
        1,
        step("b"),
    ));
    drawing_error(write(
        &mut store,
        ids,
        Target::Existing(scene),
        2,
        step("b"),
    ));
}

#[test]
fn thread_run_membership_revision_and_layout_are_guarded() {
    let scratch = Scratch::new("typed-guards");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    let other = begin(&mut store);
    drawing_error(write(
        &mut store,
        other,
        Target::Existing(scene),
        2,
        step("b"),
    ));
    assert!(matches!(
        write(
            &mut store,
            (ids.0, other.1),
            Target::Existing(scene),
            2,
            step("b")
        ),
        Err(StoreError::NoRun)
    ));
    drawing_error(write(
        &mut store,
        ids,
        Target::Existing(scene),
        3,
        step("b"),
    ));
    drawing_error(store.drawing_write(&Write::Step {
        thread: ids.0,
        run: ids.1,
        target: Target::Existing(scene),
        revision: 2,
        layout_version: 2,
        step: step("b"),
    }));
    store
        .end_run(ids.0, ids.1, &[], super::super::RunEnd::Done)
        .expect("end");
    assert!(matches!(
        write(&mut store, ids, Target::Existing(scene), 2, step("b")),
        Err(StoreError::NoRun)
    ));
    let next_run = RunId(other.1.0 + 1);
    store
        .begin_turn(ids.0, None, next_run, "continue")
        .expect("next turn");
    write(
        &mut store,
        (ids.0, next_run),
        Target::Existing(scene),
        2,
        step("b"),
    )
    .expect("cross-run continuation");
    store.begin_delete(ids.0).expect("hide");
    assert!(matches!(
        write(
            &mut store,
            (ids.0, next_run),
            Target::Existing(scene),
            3,
            step("c")
        ),
        Err(StoreError::NoThread)
    ));
    assert!(matches!(
        store.drawing_write(&Write::Lookup {
            thread: ids.0,
            name: header("scene").begin.scene
        }),
        Err(StoreError::NoThread)
    ));
    drawing_error(cut(&mut store, scene, 2, 1));
    assert!(store.drawing_scenes().expect("hidden").is_empty());
}

#[test]
fn cut_retries_are_idempotent_even_after_a_later_step() {
    let scratch = Scratch::new("typed-cut-retry");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    assert_eq!(cut(&mut store, scene, 1, 1).expect("cut"), Reply::Cut);
    assert_eq!(cut(&mut store, scene, 1, 1).expect("retry"), Reply::Cut);
    drawing_error(cut(&mut store, scene, 1, 0));
    write(&mut store, ids, Target::Existing(scene), 2, step("b")).expect("next");
    assert_eq!(cut(&mut store, scene, 1, 1).expect("old retry"), Reply::Cut);
    drawing_error(cut(&mut store, scene, 1, 0));
    drawing_error(cut(&mut store, scene, 3, 1));
    assert_eq!(count(&store, "scene_cut"), 1);
}

#[test]
fn redundant_direct_cut_is_rejected_without_a_recovery_log_event() {
    let scratch = Scratch::new("typed-redundant-cut");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    drawing_error(store.drawing_write(&Write::Cut {
        scene,
        revision: 1,
        cut: StopCut {
            activated: 1,
            revealed: Bounded::new(vec![Handle::new(1, 0).expect("handle")]).expect("handles"),
        },
    }));
    assert_eq!(count(&store, "scene_cut"), 0);
    assert_eq!(
        store.drawing_scenes().expect("unchanged log")[0].log.len(),
        1
    );
    cut(&mut store, scene, 1, 1).expect("effective cut");
    assert_eq!(count(&store, "scene_cut"), 1);
}

#[test]
fn empty_typed_history_does_not_query_threads_but_typed_history_still_fails_closed() {
    let scratch = Scratch::new("typed-empty-threads-missing");
    let store = Store::open(&scratch.db()).expect("store");
    store
        .conn
        .execute_batch("DROP TABLE threads")
        .expect("remove empty threads table");
    assert!(store.drawing_scenes().expect("no typed history").is_empty());

    let typed_scratch = Scratch::new("typed-nonempty-threads-missing");
    let mut typed_store = Store::open(&typed_scratch.db()).expect("store");
    let ids = begin(&mut typed_store);
    first(&mut typed_store, ids, "scene");
    typed_store
        .conn
        .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE threads;")
        .expect("corrupt typed store");
    drawing_error(typed_store.drawing_scenes());
}

#[test]
fn original_steps_and_cuts_replay_with_gaps_and_reserved_ids() {
    let scratch = Scratch::new("typed-replay");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    write(&mut store, ids, Target::Existing(scene), 2, step("dropped")).expect("second");
    let first_cut = StopCut {
        activated: 1,
        revealed: Bounded::new(vec![Handle::new(1, 0).expect("handle")]).expect("handles"),
    };
    store
        .drawing_write(&Write::Cut {
            scene,
            revision: 2,
            cut: first_cut.clone(),
        })
        .expect("cut");
    write(&mut store, ids, Target::Existing(scene), 3, step("c")).expect("third");
    let second_cut = StopCut {
        activated: 3,
        revealed: Bounded::new(vec![Handle::new(1, 0).expect("handle")]).expect("handles"),
    };
    store
        .drawing_write(&Write::Cut {
            scene,
            revision: 3,
            cut: second_cut.clone(),
        })
        .expect("second cut");
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let recovered = store.drawing_scenes().expect("scenes");
    assert_eq!(recovered.len(), 1);
    let expected = vec![
        SceneEvent::Step {
            revision: 1,
            step: step("a"),
        },
        SceneEvent::Step {
            revision: 2,
            step: step("dropped"),
        },
        SceneEvent::Stop(first_cut),
        SceneEvent::Step {
            revision: 3,
            step: step("c"),
        },
        SceneEvent::Stop(second_cut),
    ];
    assert_eq!(recovered[0].log, expected);
    let replayed = SceneState::replay(recovered[0].header.begin.clone(), &recovered[0].log)
        .expect("semantic replay");
    assert_eq!(replayed.recovery_log(), expected);
    assert_eq!(
        replayed.handle(&WireId::try_from("object-dropped".to_owned()).expect("id")),
        None
    );
    assert_eq!(
        replayed.handle(&WireId::try_from("object-c".to_owned()).expect("id")),
        None
    );
    let mut reused = step("dropped").as_ref().clone();
    reused.say = None;
    assert!(
        replayed.admit(&reused, &GlobalBudget::default()).is_err(),
        "a cut cannot free a reserved step id"
    );
    let Admission::New(_) = replayed
        .admit(&step("d"), &GlobalBudget::default())
        .expect("new id")
    else {
        panic!("new id must be new")
    };
}

#[test]
fn malformed_truncated_oversized_and_wrong_schema_bodies_are_content_free() {
    for body in [
        "{\"scene\":",
        "{\"scene\":2,\"revision\":\"privatecontent\"}",
        "{}",
    ] {
        let scratch = Scratch::new("typed-corrupt");
        let mut store = Store::open(&scratch.db()).expect("store");
        let ids = begin(&mut store);
        let scene = first(&mut store, ids, "scene");
        corrupt_step(&store, body);
        drawing_error(store.drawing_scenes());
        drawing_error(write(
            &mut store,
            ids,
            Target::Existing(scene),
            2,
            step("b"),
        ));
    }
    let scratch = Scratch::new("typed-oversized");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    first(&mut store, ids, "scene");
    corrupt_step(&store, &" ".repeat(MAX_BODY_BYTES + 1));
    drawing_error(store.drawing_scenes());
}

#[test]
fn header_and_step_versions_and_revisions_are_validated_on_reload() {
    for change in [
        serde_json::json!({"revision": 2}),
        serde_json::json!({"layout_version": 2}),
        serde_json::json!({"unexpected": "privatecontent"}),
        serde_json::json!({"step": {"id":"x", "say": "privatecontent", "remove":[""]}}),
    ] {
        let scratch = Scratch::new("typed-schema");
        let mut store = Store::open(&scratch.db()).expect("store");
        let ids = begin(&mut store);
        first(&mut store, ids, "scene");
        let body: String = store
            .conn
            .query_row(
                "SELECT body FROM events WHERE kind = 'scene_step'",
                [],
                |row| row.get(0),
            )
            .expect("body");
        let mut body: serde_json::Value = serde_json::from_str(&body).expect("json");
        for (key, value) in change.as_object().expect("object") {
            body[key] = value.clone();
        }
        corrupt_step(&store, &body.to_string());
        drawing_error(store.drawing_scenes());
    }
    for body in [
        "{",
        r#"{"begin":{"v":2,"scene":"scene","surface":"overlay","alt":"privatecontent"},"layout_version":1}"#,
        r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"privatecontent"},"layout_version":2}"#,
        r#"{"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"privatecontent"},"layout_version":1,"unexpected":true}"#,
    ] {
        let scratch = Scratch::new("typed-header");
        let mut store = Store::open(&scratch.db()).expect("store");
        let ids = begin(&mut store);
        first(&mut store, ids, "scene");
        store
            .conn
            .execute_batch("DROP TRIGGER events_append_only")
            .expect("mutation");
        store
            .conn
            .execute(
                "UPDATE events SET body = ?1 WHERE kind = 'scene_begin'",
                [body],
            )
            .expect("corrupt");
        drawing_error(store.drawing_scenes());
    }
}

#[test]
fn invalid_cut_handles_and_cut_sql_failures_do_not_append() {
    let scratch = Scratch::new("typed-cut-failures");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    let handle = Handle::new(1, 0).expect("handle");
    for (activated, handles) in [(2, vec![]), (0, vec![handle]), (1, vec![handle, handle])] {
        drawing_error(store.drawing_write(&Write::Cut {
            scene,
            revision: 1,
            cut: StopCut {
                activated,
                revealed: Bounded::new(handles).expect("bounded"),
            },
        }));
    }
    assert_eq!(count(&store, "scene_cut"), 0);
    store.conn.execute_batch("CREATE TRIGGER fail_cut BEFORE INSERT ON events WHEN NEW.kind = 'scene_cut' BEGIN SELECT RAISE(ABORT, 'privatecontent'); END;").expect("trap");
    drawing_error(cut(&mut store, scene, 1, 1));
    assert_eq!(count(&store, "scene_cut"), 0);
    store
        .conn
        .execute_batch("DROP TRIGGER fail_cut")
        .expect("untrap");
    cut(&mut store, scene, 1, 0).expect("zero cut");
    assert_eq!(
        count(&store, "scene_cleared"),
        0,
        "the core owns the clear debt"
    );
    assert!(
        store
            .drawing_scenes()
            .expect("terminal cut is hidden")
            .is_empty()
    );
    assert_eq!(count(&store, "scene_cut"), 1);
}

#[test]
fn zero_cut_survives_crash_before_clear_without_crowding_out_the_fourth_scene() {
    let scratch = Scratch::new("typed-zero-cut-crash");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let older: Vec<_> = (0..4)
        .map(|index| first(&mut store, ids, &format!("older-{index}")))
        .collect();
    let terminal = first(&mut store, ids, "terminal");
    cut(&mut store, terminal, 1, 0).expect("durable zero cut");
    assert_eq!(count(&store, "scene_cleared"), 0);
    drop(store);

    let mut store = Store::open(&scratch.db()).expect("reopen before clear debt commits");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("visible scenes")
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        older
    );
    assert_eq!(
        store
            .drawing_write(&Write::Lookup {
                thread: ids.0,
                name: header("terminal").begin.scene
            })
            .expect("name tombstone"),
        Reply::Lookup(Some(terminal))
    );
    drawing_error(write(
        &mut store,
        ids,
        Target::New(header("terminal")),
        1,
        step("new"),
    ));
    drawing_error(write(
        &mut store,
        ids,
        Target::Existing(terminal),
        2,
        step("later"),
    ));
    assert_eq!(
        cut(&mut store, terminal, 1, 0).expect("cut retry"),
        Reply::Cut
    );
    assert_eq!(count(&store, "scene_cut"), 1);
    assert_eq!(count(&store, "scene_cleared"), 0);
}

#[test]
fn newest_four_typed_scenes_ignore_legacy_and_cleared_begins() {
    let scratch = Scratch::new("typed-four");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scenes: Vec<_> = (0..6)
        .map(|index| first(&mut store, ids, &format!("scene-{index}")))
        .collect();
    store.clear_scenes(&[scenes[5]]).expect("clear");
    store
        .commit_step(
            ids,
            None,
            super::super::SceneTarget::New { evict: None },
            0,
            "legacy",
        )
        .expect("legacy");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("four")
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        scenes[1..5]
    );
}

#[test]
fn typed_scene_begins_do_not_crowd_legacy_recovery_during_staging() {
    let scratch = Scratch::new("typed-legacy-boundary");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let legacy = store
        .commit_step(
            ids,
            None,
            super::super::SceneTarget::New { evict: None },
            0,
            "legacy",
        )
        .expect("legacy step");
    for index in 0..4 {
        first(&mut store, ids, &format!("typed-{index}"));
    }
    let scenes = store.scenes().expect("legacy recovery");
    assert_eq!(scenes.len(), 1);
    assert_eq!(scenes[0].scene, legacy);
    assert_eq!(scenes[0].kept[0].label, "legacy");
}

#[test]
fn lifetime_log_is_bounded_before_extension_and_recovery() {
    let scratch = Scratch::new("typed-log-cap");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
    cut(&mut store, scene, 1, 1).expect("cut");
    for revision in 2..=64 {
        write(
            &mut store,
            ids,
            Target::Existing(scene),
            revision,
            step(&format!("s{revision}")),
        )
        .expect("step");
        cut(&mut store, scene, revision, revision).expect("cut");
    }
    assert_eq!(store.drawing_scenes().expect("bounded")[0].log.len(), 128);
    drawing_error(write(
        &mut store,
        ids,
        Target::Existing(scene),
        65,
        step("excess"),
    ));
    store.conn.execute("INSERT INTO events (thread,kind,body,created_ms) SELECT thread,kind,body,created_ms FROM events WHERE kind = 'scene_cut' ORDER BY id DESC LIMIT 1", []).expect("extra row");
    drawing_error(store.drawing_scenes());
}

#[test]
fn expanded_typed_json_above_wire_record_cap_reloads_exactly() {
    let scratch = Scratch::new("typed-expanded");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let mut put = Vec::new();
    for chart in 0..4 {
        put.push(serde_json::json!({"chart":{"id":format!("c{chart}"),"kind":"line","box":[0,0,100,100]}}));
        for series in 0..if chart == 3 { 7 } else { 8 } {
            put.push(serde_json::json!({"series":{"id":format!("s{chart}_{series}"),"in":format!("c{chart}"),"data":vec![0;500]}}));
        }
    }
    let wire =
        serde_json::to_vec(&serde_json::json!({"step":{"id":"dense","put":put}})).expect("wire");
    assert_eq!(wire.len(), 32_619);
    let kumochi_scene::Record::Step(dense) =
        kumochi_scene::parse_record(&wire).expect("legal record")
    else {
        panic!("step")
    };
    let state = SceneState::begin(header("scene").begin).expect("scene");
    assert!(matches!(
        state
            .admit(&dense, &GlobalBudget::default())
            .expect("semantically legal step"),
        Admission::New(_)
    ));
    let dense = Arc::new(dense);
    let Reply::Step(scene) = write(
        &mut store,
        ids,
        Target::New(header("scene")),
        1,
        Arc::clone(&dense),
    )
    .expect("write") else {
        panic!("scene")
    };
    let bytes: i64 = store
        .conn
        .query_row(
            "SELECT length(CAST(body AS BLOB)) FROM events WHERE kind = 'scene_step'",
            [],
            |row| row.get(0),
        )
        .expect("bytes");
    assert!(
        bytes
            > i64::try_from(kumochi_scene::limits::MAX_RECORD_BYTES).expect("record cap fits SQL")
    );
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store.drawing_scenes().expect("reloaded")[0].log,
        vec![SceneEvent::Step {
            revision: 1,
            step: dense
        }]
    );
    assert_eq!(store.drawing_scenes().expect("scene")[0].scene, scene);
}

#[test]
fn f64_data_recovers_without_changing_one_bit() {
    let scratch = Scratch::new("typed-f64");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let mut series =
        serde_json::from_str::<kumochi_scene::record::Series>(r#"{"id":"data","in":"chart"}"#)
            .expect("series");
    series.data = Bounded::new(vec![Point::Value(
        Number::try_from(51.248_178_375_505_404_f64).expect("finite"),
    )])
    .expect("data");
    let mut original = step("a").as_ref().clone();
    original.put = Bounded::new(vec![Op::Series(series)]).expect("ops");
    let original = Arc::new(original);
    write(
        &mut store,
        ids,
        Target::New(header("scene")),
        1,
        Arc::clone(&original),
    )
    .expect("write");
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store.drawing_scenes().expect("recovery")[0].log,
        vec![SceneEvent::Step {
            revision: 1,
            step: original
        }]
    );
}
