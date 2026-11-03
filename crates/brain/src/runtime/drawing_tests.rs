use std::cell::RefCell;
use std::sync::Arc;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{ControlToShell, DataToShell, RunId, ThreadId};
use tokio::sync::{Notify, mpsc};

use super::super::data::DataQueue;
use super::super::{Brain, Link, window};
use crate::drawing::{Reply, Target, Write};
use crate::engines::fixture::Fixture;
use crate::store::{RunEnd, Store};
use crate::tap::Tap;
use crate::test_support::Scratch;

fn stored_step(id: &str, object: &str) -> Arc<kumochi_scene::Step> {
    Arc::new(serde_json::from_value(serde_json::json!({"id":id,"put":[{"shape":{"id":object,"kind":"rect","box":[0,0,10,10]}}]})).expect("typed step"))
}

fn typed_brain(
    scratch: &Scratch,
    fault: Option<&str>,
) -> (Brain, kumochi_protocol::message::SceneId) {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main = store.main_thread().expect("main");
    let thread = ThreadId(2);
    let run = RunId(1);
    store
        .begin_turn(thread, Some("task"), run, "turn")
        .expect("turn");
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"diagram"},"layout_version":1})).expect("header");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            target: Target::New(header),
            revision: 1,
            layout_version: 1,
            step: stored_step("a", "a"),
        })
        .expect("first step")
    else {
        panic!("step reply")
    };
    store
        .drawing_write(&Write::Step {
            thread,
            run,
            target: Target::Existing(scene),
            revision: 2,
            layout_version: 1,
            step: stored_step("b", "b"),
        })
        .expect("second step");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("outcome");
    if let Some(sql) = fault {
        store
            .conn_for_tests()
            .execute_batch(sql)
            .expect("fault trigger");
    }
    let high = store.high_water().expect("high water");
    (
        Brain::new(
            store,
            main,
            high,
            Vec::new(),
            Box::new(Fixture::parse("").expect("fixture")),
            scratch.config(),
            crate::test_support::accounts(scratch),
        )
        .expect("brain"),
        scene,
    )
}

fn cut(scene: kumochi_protocol::message::SceneId) -> crate::coordinator::Input {
    crate::coordinator::Input::Drawing(crate::drawing::Input::StopScenes {
        run: RunId(1),
        cuts: kumochi_protocol::message::SceneCuts::new(vec![
            kumochi_protocol::message::SceneCut {
                scene,
                cut: kumochi_scene::StopCut {
                    activated: 2,
                    revealed: kumochi_scene::text::Bounded::new(vec![
                        kumochi_scene::Handle::new(1, 0).expect("handle"),
                    ])
                    .expect("handle list"),
                },
            },
        ])
        .expect("cut list"),
    })
}

#[tokio::test]
async fn a_corrupt_scene_is_cleared_without_losing_the_other_three_or_stopping_the_brain() {
    let scratch = Scratch::new("typed-recovery-bad-scene");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main = store.main_thread().expect("main");
    let mut scenes = Vec::new();
    for index in 1..=4 {
        let thread = ThreadId(index + 1);
        let run = RunId(index);
        store
            .begin_turn(thread, Some("task"), run, "turn")
            .expect("turn");
        let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":format!("s{index}"),"surface":"overlay","alt":"diagram"},"layout_version":1})).expect("header");
        let step = Arc::new(serde_json::from_value(serde_json::json!({"id":"a","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]})).expect("step"));
        let Reply::Step(scene) = store
            .drawing_write(&Write::Step {
                thread,
                run,
                target: Target::New(header),
                revision: 1,
                layout_version: 1,
                step,
            })
            .expect("durable step")
        else {
            panic!("step reply")
        };
        scenes.push(scene);
        store
            .end_run(thread, run, &[], RunEnd::Done)
            .expect("outcome");
    }
    store
        .conn_for_tests()
        .execute_batch("DROP TRIGGER events_append_only")
        .expect("allow fixture corruption");
    store
        .conn_for_tests()
        .execute(
            "UPDATE events SET body='{private drawing' WHERE kind='scene_step' AND thread=2",
            [],
        )
        .expect("corrupt one scene");
    let high = store.high_water().expect("high water");
    let mut brain = Brain::new(
        store,
        main,
        high,
        Vec::new(),
        Box::new(Fixture::parse("").expect("fixture")),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("drawing corruption must not stop the brain");
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), brain.inputs.recv())
        .await
        .expect("durable recovery clear deadline")
        .expect("clear acknowledgement");
    assert!(
        matches!(&reply,crate::coordinator::Input::ScenesCleared {scenes:cleared,ok:true} if cleared == &scenes[..1])
    );
    brain.offline(reply);
    assert!(brain.coordinator.settled());
    assert_eq!(
        brain
            .coordinator
            .drawing_snapshot()
            .expect("snapshot")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        scenes[1..]
    );
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("restart");
    let recovery = store.drawing_scenes().expect("recovery after clear");
    assert!(recovery.skipped.is_empty());
    assert_eq!(
        recovery
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        scenes[1..]
    );
    assert_eq!(
        store
            .conn_for_tests()
            .query_row(
                "SELECT count(*) FROM events WHERE kind='scene_begin'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .expect("retained scene history"),
        4
    );
}

#[tokio::test]
async fn quit_drains_a_held_typed_cut_then_confirms_its_clear_before_success() {
    let scratch = Scratch::new("typed-quit-held-cut");
    let (mut brain, scene) = typed_brain(&scratch, None);
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(
            op,
            crate::coordinator::StoreOp::Drawing {
                write: Write::Cut { .. },
                ..
            }
        )
    });
    brain.offline(cut(scene));
    let quit = brain.quit(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
    let released = async {
        while !matches!(
            observed.recv().await.expect("quit observation"),
            super::super::Seen::QuitDraining
        ) {}
        release.send(()).expect("release cut");
    };
    let (quit, ()) = tokio::join!(quit, released);
    assert_eq!(quit, super::super::Quit::Complete);
    let store = Store::open(&scratch.db()).expect("restart");
    assert!(
        store
            .drawing_scenes()
            .expect("no resurrected typed scene")
            .scenes
            .is_empty()
    );
}

#[tokio::test]
async fn a_refused_snapshot_scene_is_durably_cleared_before_the_other_scene_is_described() {
    let scratch = Scratch::new("typed-producer-recovery");
    let (mut brain, good) = typed_brain(&scratch, None);
    let mut store = Store::open(&scratch.db()).expect("store");
    let main = store.main_thread().expect("main");
    let thread = ThreadId(3);
    let run = RunId(2);
    store
        .begin_turn(thread, Some("task"), run, "turn")
        .expect("turn");
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"bad","surface":"overlay","alt":"diagram"},"layout_version":1})).expect("header");
    let step = Arc::new(serde_json::from_value(serde_json::json!({"id":"a","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]})).expect("step"));
    let Reply::Step(bad) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            target: Target::New(header),
            revision: 1,
            layout_version: 1,
            step,
        })
        .expect("step")
    else {
        panic!("step reply")
    };
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("outcome");
    let mut recovery = store.drawing_scenes().expect("stored scenes");
    recovery
        .scenes
        .iter_mut()
        .find(|scene| scene.scene == bad)
        .expect("bad producer scene")
        .log
        .clear();
    let (last_thread, last_run) = store.high_water().expect("high water");
    brain.coordinator =
        crate::coordinator::Coordinator::new(main, last_thread, last_run, Vec::new());
    assert!(brain.coordinator.restore_drawings(Ok(recovery)).is_empty());
    drop(store);
    let (out, mut controls) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert!(
        brain
            .describe(&mut link)
            .expect("producer recovery does not break the connection")
            .is_none()
    );
    assert!(controls.try_recv().is_err());
    assert_eq!(queue.borrow().bytes(), 0);
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), brain.inputs.recv())
        .await
        .expect("clear deadline")
        .expect("clear reply");
    assert!(
        matches!(&reply,crate::coordinator::Input::ScenesCleared {scenes,ok:true} if scenes == &[bad])
    );
    brain.offline(reply);
    assert!(
        brain
            .describe(&mut link)
            .expect("recovery after the clear")
            .is_some()
    );
    let message = controls.try_recv().expect("membership after the clear");
    let membership = frame::read::<ControlToShell>(&mut message.as_slice(), Pipe::Control)
        .expect("membership frame")
        .expect("membership");
    assert!(
        matches!(membership,ControlToShell::ScenesV1 {scenes} if scenes.iter().map(|scene|scene.scene).collect::<Vec<_>>() == [good])
    );
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("restart");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("cleared producer scene stays absent")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [good]
    );
}

#[tokio::test]
async fn an_unavailable_drawing_read_starts_the_brain_but_cannot_confirm_quit() {
    let scratch = Scratch::new("typed-recovery-read-failure");
    let (brain, scene) = typed_brain(&scratch, None);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("store");
    let main = store.main_thread().expect("main");
    let high = store.high_water().expect("high water");
    store
        .conn_for_tests()
        .execute_batch("ALTER TABLE threads RENAME TO unreadable_threads")
        .expect("make drawing read fail");
    let brain = Brain::new(
        store,
        main,
        high,
        Vec::new(),
        Box::new(Fixture::parse("").expect("fixture")),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("a failed drawing query must not stop the brain");
    assert!(!brain.coordinator.drawing_available());
    let repair = rusqlite::Connection::open(scratch.db()).expect("repair connection");
    repair
        .execute_batch("ALTER TABLE unreadable_threads RENAME TO threads")
        .expect("restore reads");
    drop(repair);
    assert_eq!(
        brain
            .quit(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
            .await,
        super::super::Quit::Incomplete
    );
    let store = Store::open(&scratch.db()).expect("restart");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("unknown durable scene retained")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [scene]
    );
}

#[tokio::test]
async fn quit_supersedes_failed_typed_cut_debt_and_durably_clears_the_scene() {
    let scratch = Scratch::new("typed-quit-failed-cut");
    let (mut brain, scene) = typed_brain(
        &scratch,
        Some(
            "CREATE TRIGGER fail_cut BEFORE INSERT ON events WHEN new.kind='scene_cut' BEGIN SELECT RAISE(FAIL,'private drawing'); END;",
        ),
    );
    brain.offline(cut(scene));
    let failed = tokio::time::timeout(std::time::Duration::from_secs(2), brain.inputs.recv())
        .await
        .expect("cut reply before timeout")
        .expect("cut reply");
    assert!(matches!(
        failed,
        crate::coordinator::Input::DrawingWritten { reply: Err(()), .. }
    ));
    brain.offline(failed);
    assert_eq!(
        brain
            .quit(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
            .await,
        super::super::Quit::Complete
    );
    let store = Store::open(&scratch.db()).expect("restart");
    assert!(
        store
            .drawing_scenes()
            .expect("clear superseded debt")
            .scenes
            .is_empty()
    );
}

#[tokio::test]
async fn quit_with_an_unconfirmed_typed_clear_is_incomplete() {
    let scratch = Scratch::new("typed-quit-failed-clear");
    let (brain, _) = typed_brain(
        &scratch,
        Some(
            "CREATE TRIGGER fail_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' BEGIN SELECT RAISE(FAIL,'private drawing'); END;",
        ),
    );
    assert_eq!(
        brain
            .quit(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
            .await,
        super::super::Quit::Incomplete
    );
    let store = Store::open(&scratch.db()).expect("restart");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("uncleared scene retained")
            .scenes
            .len(),
        1
    );
}

#[tokio::test]
async fn a_typed_restart_describes_and_queues_the_original_log_with_scene_ownership() {
    let scratch = Scratch::new("drawing-runtime-recovery");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovery");
    let main = store.main_thread().expect("main");
    let thread = ThreadId(2);
    let run = RunId(1);
    store
        .begin_turn(thread, Some("task"), run, "turn")
        .expect("turn");
    let header = serde_json::from_value(serde_json::json!({
        "begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"},
        "layout_version":1
    }))
    .expect("typed header");
    let step = stored_step("first", "box");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            target: Target::New(header),
            revision: 1,
            layout_version: 1,
            step: Arc::clone(&step),
        })
        .expect("durable step")
    else {
        panic!("step reply")
    };
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("end run");
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen");
    store.recover().expect("restart scan");
    let high = store.high_water().expect("high water");
    let mut brain = Brain::new(
        store,
        main,
        high,
        Vec::new(),
        Box::new(Fixture::parse("").expect("fixture")),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("brain");
    let (out, mut controls) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    brain
        .apply(
            vec![crate::coordinator::Effect::Data(
                DataToShell::SceneStepCommit {
                    run,
                    scene,
                    index: 0,
                    step: (*step).clone(),
                },
            )],
            Some(&mut link),
        )
        .expect("late old-connection commit is recovered, not sent twice");
    assert_eq!(queue.borrow().bytes(), 0);
    let mut recovery = brain
        .describe(&mut link)
        .expect("validated recovery")
        .expect("no repairs needed");
    let control = controls.try_recv().expect("membership");
    let ControlToShell::ScenesV1 { scenes } =
        frame::read::<ControlToShell>(&mut control.as_slice(), Pipe::Control)
            .expect("decode")
            .expect("frame")
    else {
        panic!("v1 membership")
    };
    assert_eq!(scenes.len(), 1);
    assert_eq!(scenes[0].scene, scene);
    assert_eq!(scenes[0].revision, 1);
    let message = recovery.next_chunk().expect("whole log");
    let DataToShell::SceneSnapshotV1 { chunk } = &message else {
        panic!("v1 snapshot")
    };
    assert_eq!(
        &chunk.events[..],
        &[kumochi_scene::SceneEvent::Step { revision: 1, step }]
    );
    assert!(chunk.last);
    assert!(!recovery.pending());
    link.data(&message).expect("queued");
    let before = queue.borrow().bytes();
    assert!(before > 0);
    queue.borrow_mut().revoke(run);
    assert_eq!(queue.borrow().bytes(), before);
    queue.borrow_mut().revoke_scenes(&[scene]);
    assert_eq!(queue.borrow().bytes(), 0);
    brain.close().await;
}
