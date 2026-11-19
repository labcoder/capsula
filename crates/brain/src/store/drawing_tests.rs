use std::{error::Error as _, sync::Arc};

use kumochi_protocol::message::{MAX_SCENE_LOG_ENTRIES, RunId, SceneHeader, SceneId, ThreadId};
use kumochi_scene::{
    Admission, GlobalBudget, Handle, Op, SceneEvent, SceneState, Step, StopCut, WireId,
    record::{Point, Surface},
    text::Bounded,
};
use rusqlite::params;

use super::{DrawingRecovery, MAX_BODY_BYTES, Store, StoreError};
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
fn measure_dense_scene_append_and_cut_at_lifetime_cap() {
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
        let start = Instant::now();
        cut(&mut store, scene, 64, 64).expect("cap cut");
        let cut_ns = start.elapsed().as_nanos();
        let stored = store.drawing_scenes().expect("full log");
        let stored = &stored.scenes[0];
        assert_eq!(stored.log.len(), 65);
        let replayed = SceneState::replay(stored.header.begin.clone(), &stored.log)
            .expect("valid canonical scene");
        assert_eq!(replayed.revision(), 64);
        if sample >= 8 {
            samples.push((sample - 8, first_ns, cap_ns, cut_ns));
        }
    }
    println!(
        "dense_scene_append_and_cut points=1600 lifetime_steps=64 cut_revision=64 warmups=8 samples=8 includes_transaction=true"
    );
    for (sample, first_ns, cap_ns, cut_ns) in samples {
        println!("sample,{sample},{first_ns},{cap_ns},{cut_ns}");
    }
}

fn header(name: &str) -> SceneHeader {
    serde_json::from_value(serde_json::json!({
        "begin": {"v":1,"scene":name,"surface":"overlay","alt":"privatealt"},
        "layout_version":2
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
        clear: Vec::new(),
        target,
        revision,
        layout_version: 2,
        chat: None,
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

#[test]
fn replacement_clear_lists_are_bounded_unique_and_only_for_new_scenes() {
    let scratch = Scratch::new("drawing-replacement-list");
    let mut store = Store::open(&scratch.db()).expect("store");
    let old: Vec<_> = (0..5)
        .map(|n| {
            let ids = begin(&mut store);
            first(&mut store, ids, &format!("old{n}"))
        })
        .collect();
    let ids = begin(&mut store);
    for (target, clear) in [
        (Target::New(header("new")), old.clone()),
        (Target::New(header("new")), vec![old[0], old[0]]),
        (Target::Existing(old[0]), vec![old[1]]),
    ] {
        assert!(matches!(
            store.drawing_write(&Write::Step {
                thread: ids.0,
                run: ids.1,
                target,
                clear,
                revision: 1,
                layout_version: 2,
                chat: None,
                step: step("first"),
            }),
            Err(StoreError::Drawing)
        ));
        assert_eq!(count(&store, "scene_begin"), 5);
        assert_eq!(count(&store, "scene_step"), 5);
        assert_eq!(count(&store, "scene_cleared"), 0);
    }
    let Reply::Step(new) = store
        .drawing_write(&Write::Step {
            thread: ids.0,
            run: ids.1,
            target: Target::New(header("new")),
            clear: old[..4].to_vec(),
            revision: 1,
            layout_version: 2,
            chat: None,
            step: step("first"),
        })
        .expect("four distinct victims are accepted")
    else {
        panic!("step reply")
    };
    assert_eq!(count(&store, "scene_cleared"), 4);
    assert_eq!(
        store
            .drawing_scenes()
            .expect("replacement")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [old[4], new]
    );
}

#[test]
fn a_selected_clear_failure_rolls_back_the_entire_replacement_transaction() {
    let scratch = Scratch::new("drawing-replacement-rollback");
    let mut store = Store::open(&scratch.db()).expect("store");
    let first_thread = begin(&mut store);
    let old_one = first(&mut store, first_thread, "old_one");
    let second_thread = begin(&mut store);
    let old_two = first(&mut store, second_thread, "old_two");
    let ids = begin(&mut store);
    let before: i64 = store
        .conn_for_tests()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .expect("count");
    store.conn_for_tests().execute_batch(&format!(
        "CREATE TRIGGER fail_selected_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' AND json_extract(new.body,'$.scene')={} BEGIN SELECT RAISE(FAIL,'private replacement'); END;", old_two.0
    )).expect("failure trigger");
    let error = store
        .drawing_write(&Write::Step {
            thread: ids.0,
            run: ids.1,
            target: Target::New(header("new")),
            clear: vec![old_one, old_two],
            revision: 1,
            layout_version: 2,
            chat: None,
            step: step("first"),
        })
        .expect_err("selected clear fails");
    assert!(!format!("{error:?}").contains("private replacement"));
    let after: i64 = store
        .conn_for_tests()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .expect("count");
    assert_eq!(after, before);
    assert_eq!(
        store
            .drawing_write(&Write::Lookup {
                thread: ids.0,
                surface: Surface::Overlay,
                name: "new".to_owned().try_into().expect("id")
            })
            .expect("lookup"),
        Reply::Lookup(None)
    );
    assert_eq!(
        store
            .drawing_scenes()
            .expect("old set")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [old_one, old_two]
    );
}

#[test]
fn recovery_reconciles_a_committed_replacement_without_a_delivered_reply() {
    let scratch = Scratch::new("drawing-replacement-lost-reply");
    let mut store = Store::open(&scratch.db()).expect("store");
    let first_thread = begin(&mut store);
    let old_one = first(&mut store, first_thread, "old_one");
    let second_thread = begin(&mut store);
    let old_two = first(&mut store, second_thread, "old_two");
    let ids = begin(&mut store);
    let Reply::Step(new) = store
        .drawing_write(&Write::Step {
            thread: ids.0,
            run: ids.1,
            target: Target::New(header("new")),
            clear: vec![old_one, old_two],
            revision: 1,
            layout_version: 2,
            chat: None,
            step: step("first"),
        })
        .expect("committed before losing delivery")
    else {
        panic!("step reply")
    };
    let rows: Vec<(i64, i64)> = store.conn_for_tests().prepare(
        "SELECT thread, json_extract(body,'$.scene') FROM events WHERE kind='scene_cleared' ORDER BY id"
    ).expect("clear rows").query_map([], |row| Ok((row.get(0)?,row.get(1)?))).expect("rows")
        .collect::<Result<_,_>>().expect("typed rows");
    assert_eq!(
        rows,
        [
            (
                i64::try_from(first_thread.0.0).expect("thread"),
                i64::try_from(old_one.0).expect("scene")
            ),
            (
                i64::try_from(second_thread.0.0).expect("thread"),
                i64::try_from(old_two.0).expect("scene")
            )
        ]
    );
    drop(store);
    let store = Store::open(&scratch.db()).expect("restart without retrying the call");
    let recovered = store.drawing_scenes().expect("recovery");
    assert_eq!(
        recovered
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [new]
    );
    assert_eq!(recovered.scenes[0].log.len(), 1);
    assert_eq!(recovered.scenes[0].thread, ids.0);
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
    assert_eq!(
        store.drawing_scenes().expect("empty"),
        DrawingRecovery::default()
    );
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
fn first_writes_and_recovery_keep_same_names_on_distinct_surfaces() {
    for first_surface in [Surface::Overlay, Surface::Screen] {
        let scratch = Scratch::new("typed-names-reopen");
        let mut store = Store::open(&scratch.db()).expect("store");
        let ids = begin(&mut store);
        let mut overlay = header("shared");
        overlay.begin.others = kumochi_scene::Others::Keep;
        let mut screen = overlay.clone();
        screen.begin.surface = Surface::Screen;
        let ordered = if first_surface == Surface::Overlay {
            [&overlay, &screen]
        } else {
            [&screen, &overlay]
        };
        let first_writes = ordered.map(|header| {
            let Reply::Step(scene) = write(
                &mut store,
                ids,
                Target::New(header.clone()),
                1,
                step("first"),
            )
            .expect("each surface has its own first write") else {
                panic!("step reply");
            };
            (scene, header.begin.surface)
        });
        assert_ne!(first_writes[0].0, first_writes[1].0);
        drop(store);

        let mut store = Store::open(&scratch.db()).expect("reopen");
        let recovered = store.drawing_scenes().expect("both stored logs");
        assert_eq!(
            recovered
                .scenes
                .iter()
                .map(|scene| (scene.scene, scene.header.begin.surface))
                .collect::<Vec<_>>(),
            first_writes
        );
        let (overlay_id, screen_id) = if first_surface == Surface::Overlay {
            (first_writes[0].0, first_writes[1].0)
        } else {
            (first_writes[1].0, first_writes[0].0)
        };
        let (drawing, skipped) = crate::drawing::Drawing::recover(recovered.scenes);
        assert_eq!(
            skipped,
            [screen_id],
            "only the screen scene lacks a picture to replay"
        );
        assert_eq!(
            drawing
                .snapshot()
                .scenes
                .iter()
                .map(|scene| scene.scene)
                .collect::<Vec<_>>(),
            [overlay_id]
        );

        for (header, scene) in [(&overlay, overlay_id), (&screen, screen_id)] {
            let lookup = Write::Lookup {
                thread: ids.0,
                surface: header.begin.surface,
                name: header.begin.scene.clone(),
            };
            assert_eq!(
                store.drawing_write(&lookup).expect("live name"),
                Reply::Lookup(Some(scene))
            );
            assert_eq!(
                write(
                    &mut store,
                    ids,
                    Target::Existing(scene),
                    2,
                    step("continued")
                )
                .expect("continuation"),
                Reply::Step(scene)
            );
            store.clear_scenes(&[scene]).expect("clear");
            assert_eq!(
                store.drawing_write(&lookup).expect("cleared name"),
                Reply::Lookup(Some(scene))
            );
            drawing_error(write(
                &mut store,
                ids,
                Target::New(header.clone()),
                1,
                step("new"),
            ));
        }
        assert_eq!(
            store.drawing_scenes().expect("cleared"),
            DrawingRecovery::default()
        );
    }
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
                surface: Surface::Overlay,
                name: WireId::try_from("used".to_owned()).expect("name")
            })
            .expect("lookup"),
        Reply::Lookup(Some(scene))
    );
    assert_eq!(
        store.drawing_scenes().expect("recovery"),
        DrawingRecovery::default()
    );
    drawing_error(write(
        &mut store,
        ids,
        Target::New(header("used")),
        1,
        step("b"),
    ));
}

#[test]
fn thread_and_run_membership_are_guarded() {
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
            surface: Surface::Overlay,
            name: header("scene").begin.scene
        }),
        Err(StoreError::NoThread)
    ));
    drawing_error(cut(&mut store, scene, 2, 1));
    assert_eq!(
        store.drawing_scenes().expect("hidden"),
        DrawingRecovery::default()
    );
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
    assert_eq!(count(&store, "scene_cut"), 1);
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
    let recovered = store.drawing_scenes().expect("scenes").scenes;
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

/// Scenes in two threads, oldest first: one in the second thread, two in the first, then one
/// more in the second.
fn four_scenes() -> (Scratch, Store, [SceneId; 4]) {
    let scratch = Scratch::new("typed-isolation");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (one, two) = (begin(&mut store), begin(&mut store));
    let scenes = [
        first(&mut store, two, "b1"),
        first(&mut store, one, "a1"),
        first(&mut store, one, "a2"),
        first(&mut store, two, "b2"),
    ];
    (scratch, store, scenes)
}

/// The recovered scenes and the skipped ones.
fn recovered_ids(store: &Store) -> (Vec<SceneId>, Vec<SceneId>) {
    let recovery = store.drawing_scenes().expect("recovery");
    let scenes = recovery.scenes.iter().map(|scene| scene.scene).collect();
    (scenes, recovery.skipped)
}

/// A step body whose `scene` field reads as `value`.
fn naming(body: &str, value: &str) -> String {
    format!(
        "{{\"scene\":{value}{}",
        &body[body.find(',').expect("more fields")..]
    )
}

#[test]
fn a_bad_scene_is_left_out_while_the_other_three_recover() {
    type Rewrite = fn(&str) -> String;
    let rewrites: [(&str, Rewrite); 10] = [
        ("scene_begin", |body| {
            body.replace(r#""layout_version":2"#, r#""layout_version":1"#)
        }),
        ("scene_begin", |_| "{".to_owned()),
        ("scene_begin", |_| " ".repeat(MAX_BODY_BYTES + 1)),
        ("scene_step", |body| {
            body.replace(r#""revision":1"#, r#""revision":2"#)
        }),
        ("scene_step", |body| {
            body.replace(r#""step":"#, r#""extra":0,"step":"#)
        }),
        // The rest name no scene, so they fail only the scenes their thread began before them.
        ("scene_step", |_| r#"{"scene":"#.to_owned()),
        ("scene_step", |_| " ".repeat(MAX_BODY_BYTES + 1)),
        ("scene_step", |body| naming(body, r#""privatecontent""#)),
        ("scene_step", |body| naming(body, "99999999999999999999")),
        ("scene_step", |body| naming(body, "0")),
    ];
    for (kind, rewrite) in rewrites {
        let (_scratch, store, scenes) = four_scenes();
        let (row, body): (i64, String) = store
            .conn
            .query_row(
                "SELECT id, body FROM events WHERE kind = ?1 AND id >= ?2 ORDER BY id LIMIT 1",
                params![kind, i64::try_from(scenes[1].0).expect("row id")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("scene row");
        store
            .conn
            .execute_batch("DROP TRIGGER events_append_only")
            .expect("allow fixture mutation");
        store
            .conn
            .execute(
                "UPDATE events SET body = ?1 WHERE id = ?2",
                params![rewrite(&body), row],
            )
            .expect("rewrite");
        let captured = crate::test_support::Captured::default();
        let guard = captured.install();
        assert_eq!(
            recovered_ids(&store),
            (vec![scenes[0], scenes[2], scenes[3]], vec![scenes[1]])
        );
        drop(guard);
        let warning = captured.text();
        assert!(warning.contains(&format!("stored scene row {row} doesn't decode")));
        assert!(warning.contains("skipped while recovering drawings"));
        assert!(!warning.contains("privatecontent"));
        assert!(!warning.contains("privatealt"));
        assert!(!warning.contains("privatenarration"));
    }
    let (_scratch, mut store, scenes) = four_scenes();
    cut(&mut store, scenes[1], 1, 1).expect("cut");
    let copy = "INSERT INTO events (thread, kind, body, created_ms)
         SELECT thread, kind, body, created_ms FROM events WHERE kind = 'scene_cut'
         ORDER BY id LIMIT 1";
    for _ in 2..MAX_SCENE_LOG_ENTRIES {
        store.conn.execute(copy, []).expect("copy");
    }
    let full = store.drawing_scenes().expect("full log");
    assert!(full.skipped.is_empty());
    assert_eq!(full.scenes[1].log.len(), MAX_SCENE_LOG_ENTRIES);
    store.conn.execute(copy, []).expect("copy past the bound");
    assert_eq!(
        recovered_ids(&store),
        (vec![scenes[0], scenes[2], scenes[3]], vec![scenes[1]])
    );
}

#[test]
fn aggregate_body_limit_accepts_exactly_32_mib_and_skips_only_one_scene_at_one_over() {
    use kumochi_protocol::message::MAX_SCENE_SNAPSHOT_BYTES;

    let (_scratch, mut store, scenes) = four_scenes();
    cut(&mut store, scenes[1], 1, 1).expect("valid cut");
    let (row, body): (i64, String) = store
        .conn
        .query_row(
            "SELECT id, body FROM events WHERE kind='scene_cut' ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("cut body");
    let scene_row = i64::try_from(scenes[1].0).expect("native scene row");
    let step_bytes: i64 = store
        .conn
        .query_row(
            "SELECT octet_length(body) FROM events WHERE kind='scene_step' AND json_extract(body,'$.scene')=?1",
            [scene_row],
            |row| row.get(0),
        )
        .expect("step bytes");
    store
        .conn
        .execute_batch("DROP TRIGGER events_append_only")
        .expect("allow padding");
    let remaining = MAX_SCENE_SNAPSHOT_BYTES - usize::try_from(step_bytes).expect("body bytes");
    let mut last = row;
    for index in 0..5 {
        let size = remaining / 5 + usize::from(index < remaining % 5);
        assert!(size < MAX_BODY_BYTES);
        let padded = format!("{body}{}", " ".repeat(size - body.len()));
        let _: super::CutBody = serde_json::from_str(&padded).expect("individually valid body");
        if index == 0 {
            store
                .conn
                .execute(
                    "UPDATE events SET body=?1 WHERE id=?2",
                    params![padded, row],
                )
                .expect("pad first cut");
        } else {
            store.conn.execute(
                "INSERT INTO events (thread,kind,body,created_ms) SELECT thread,kind,?1,created_ms FROM events WHERE id=?2",
                params![padded, row],
            ).expect("append legal cut");
            last = store.conn.last_insert_rowid();
        }
    }
    let total: i64 = store
        .conn
        .query_row(
            "SELECT sum(octet_length(body)) FROM events WHERE kind IN ('scene_step','scene_cut') AND json_extract(body,'$.scene')=?1",
            [scene_row],
            |row| row.get(0),
        )
        .expect("aggregate bytes");
    assert_eq!(
        total,
        i64::try_from(MAX_SCENE_SNAPSHOT_BYTES).expect("byte cap")
    );
    let full = store.drawing_scenes().expect("exact boundary");
    assert_eq!(full.skipped, []);
    assert_eq!(
        full.scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        scenes
    );
    assert_eq!(full.scenes[1].log.len(), 6);
    store
        .conn
        .execute("UPDATE events SET body=body || ' ' WHERE id=?1", [last])
        .expect("one byte over");
    assert_eq!(
        recovered_ids(&store),
        (vec![scenes[0], scenes[2], scenes[3]], vec![scenes[1]])
    );
}

#[test]
fn cut_sql_failures_do_not_append() {
    let scratch = Scratch::new("typed-cut-failures");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    let scene = first(&mut store, ids, "scene");
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
    assert_eq!(
        store.drawing_scenes().expect("terminal cut is hidden"),
        DrawingRecovery::default()
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
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        older
    );
    assert_eq!(
        store
            .drawing_write(&Write::Lookup {
                thread: ids.0,
                surface: Surface::Overlay,
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
            .scenes
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
        store.drawing_scenes().expect("reloaded").scenes[0].log,
        vec![SceneEvent::Step {
            revision: 1,
            step: dense
        }]
    );
    assert_eq!(
        store.drawing_scenes().expect("scene").scenes[0].scene,
        scene
    );
}

#[test]
fn f64_data_from_model_text_recovers_without_changing_one_bit() {
    let values = [
        "51.248178375505404",
        "1.7976931348623157e308",
        "5e-324",
        "-0.0",
    ];
    let record = [
        r#"{"step":{"id":"a","put":[{"chart":{"id":"c","kind":"line","box":[0,0,100,100]}},"#,
        r#"{"series":{"id":"s","in":"c","data":["#,
        values.join(",").as_str(),
        "]}}]}}",
    ]
    .concat();
    let kumochi_scene::Record::Step(parsed) =
        kumochi_scene::parse_record(record.as_bytes()).expect("legal record")
    else {
        panic!("step")
    };
    let scratch = Scratch::new("typed-f64");
    let mut store = Store::open(&scratch.db()).expect("store");
    let ids = begin(&mut store);
    write(
        &mut store,
        ids,
        Target::New(header("scene")),
        1,
        Arc::new(parsed),
    )
    .expect("write");
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    let recovery = store.drawing_scenes().expect("recovery");
    let SceneEvent::Step { step, .. } = &recovery.scenes[0].log[0] else {
        panic!("step")
    };
    let Op::Series(series) = &step.put[1] else {
        panic!("series")
    };
    let bits: Vec<u64> = series
        .data
        .iter()
        .map(|point| match point {
            Point::Value(value) => value.get().to_bits(),
            Point::Pair(_) => panic!("value"),
        })
        .collect();
    let expected: Vec<u64> = values
        .iter()
        .map(|value| value.parse::<f64>().expect("std parse").to_bits())
        .collect();
    assert_eq!(bits, expected);
}
