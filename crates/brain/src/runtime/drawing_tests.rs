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

#[tokio::test(start_paused = true)]
async fn tuned_drawing_readiness_timer_arms_for_its_configured_wait() {
    use crate::coordinator::Effect;
    use std::time::Duration;
    use tokio::time::Instant;

    let scratch = Scratch::new("typed-drawing-ready-wait");
    std::fs::create_dir_all(scratch.dir()).unwrap();
    std::fs::write(
        scratch.dir().join(kumochi_protocol::constants::CONFIG_FILE),
        "[tuning.drawing]\nready_wait_ms = 375",
    )
    .unwrap();
    let (mut brain, _) = typed_brain(&scratch, None);
    let now = Instant::now();
    brain.apply(vec![Effect::ArmReadyWait], None).unwrap();
    assert_eq!(brain.ready_at, Some(now + Duration::from_millis(375)));
    brain.apply(vec![Effect::CancelReadyWait], None).unwrap();
    assert_eq!(brain.ready_at, None);
}

fn stored_step(id: &str, object: &str) -> Arc<kumochi_scene::Step> {
    Arc::new(serde_json::from_value(serde_json::json!({"id":id,"put":[{"shape":{"id":object,"kind":"rect","box":[0,0,10,10]}}]})).expect("typed step"))
}

fn typed_brain(
    scratch: &Scratch,
    fault: Option<&str>,
) -> (Brain, kumochi_protocol::message::SceneId) {
    typed_brain_with_steps(scratch, fault, &[])
}

fn typed_brain_with_steps(
    scratch: &Scratch,
    fault: Option<&str>,
    extra: &[Arc<kumochi_scene::Step>],
) -> (Brain, kumochi_protocol::message::SceneId) {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main = store.main_thread().expect("main");
    let thread = ThreadId(2);
    let run = RunId(1);
    store
        .begin_turn(thread, Some("task"), run, "turn")
        .expect("turn");
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"scene","surface":"overlay","alt":"diagram"},"layout_version":2})).expect("header");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step: stored_step("a", "a"),
            protected: false,
        })
        .expect("first step")
    else {
        panic!("step reply")
    };
    store
        .drawing_write(&Write::Step {
            thread,
            run,
            clear: Vec::new(),
            target: Target::Existing(scene),
            revision: 2,
            layout_version: 2,
            step: stored_step("b", "b"),
            protected: false,
        })
        .expect("second step");
    for (revision, step) in (3..).zip(extra) {
        store
            .drawing_write(&Write::Step {
                thread,
                run,
                clear: Vec::new(),
                target: Target::Existing(scene),
                revision,
                layout_version: 2,
                step: Arc::clone(step),
                protected: false,
            })
            .expect("extra recovery step");
    }
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
    let custody = crate::test_support::custody(&store);
    (
        Brain::new(
            store,
            custody,
            main,
            high,
            Vec::new(),
            crate::runtime::Selected::Fixture(Box::new(Fixture::parse("").expect("fixture"))),
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

fn four_typed_scenes(
    scratch: &Scratch,
) -> (Store, ThreadId, Vec<kumochi_protocol::message::SceneId>) {
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
        let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":format!("s{index}"),"surface":"overlay","alt":"diagram"},"layout_version":2})).expect("header");
        let step = Arc::new(serde_json::from_value(serde_json::json!({"id":"a","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]})).expect("step"));
        let Reply::Step(scene) = store
            .drawing_write(&Write::Step {
                thread,
                run,
                clear: Vec::new(),
                target: Target::New(header),
                revision: 1,
                layout_version: 2,
                step,
                protected: false,
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
    (store, main, scenes)
}

#[tokio::test]
async fn a_corrupt_scene_is_cleared_without_losing_the_other_three_or_stopping_the_brain() {
    let scratch = Scratch::new("typed-recovery-bad-scene");
    let (store, main, scenes) = four_typed_scenes(&scratch);
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
    let rows: Vec<i64> = store
        .conn_for_tests()
        .prepare("SELECT id FROM events WHERE kind='scene_step' AND thread=2")
        .expect("corrupt rows")
        .query_map([], |row| row.get(0))
        .expect("row ids")
        .collect::<Result<_, _>>()
        .expect("native ids");
    let high = store.high_water().expect("high water");
    let captured = crate::test_support::Captured::default();
    let guard = captured.install();
    let custody = crate::test_support::custody(&store);
    let mut brain = Brain::new(
        store,
        custody,
        main,
        high,
        Vec::new(),
        crate::runtime::Selected::Fixture(Box::new(Fixture::parse("").expect("fixture"))),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("drawing corruption must not stop the brain");
    drop(guard);
    let warnings = captured.text();
    for row in rows {
        assert!(warnings.contains(&format!("stored scene row {row} doesn't decode")));
    }
    assert!(warnings.contains(&format!("row={}", scenes[0].0)));
    assert!(!warnings.contains("private drawing"));
    let reply = tokio::time::timeout(super::PATIENCE, brain.inputs.recv())
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
async fn delivered_final_recovery_chunk_starts_the_crash_guard_clock() {
    let scratch = Scratch::new("typed-crash-clock-wiring");
    let (mut brain, scene) = typed_brain(&scratch, None);
    brain.crashes.plan(std::time::Instant::now());
    let ((control, data), mut shell) = super::duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let Some(ControlToShell::ScenesV1 { scenes }) = shell.control().await else {
            panic!("typed membership")
        };
        assert_eq!(
            scenes.iter().map(|named| named.scene).collect::<Vec<_>>(),
            [scene]
        );
        let DataToShell::SceneSnapshotV1 { chunk } = shell.data().await else {
            panic!("typed recovery chunk")
        };
        assert_eq!(chunk.scene, scene);
        assert!(chunk.last);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, super::super::Ended::Lost);
    let late = std::time::Instant::now() + std::time::Duration::from_secs(11);
    let plan = brain.crashes.plan(late);
    assert_eq!(plan.delay, Some(std::time::Duration::from_secs(1)));
    assert_eq!(plan.clear, []);
    brain.close().await;
}

#[tokio::test]
async fn delivered_membership_stays_armed_while_recovery_cannot_finish_queueing() {
    let scratch = Scratch::new("typed-crash-queued-wiring");
    let mut put = vec![serde_json::json!({"chart":{"id":"c","kind":"line","box":[0,0,100,100]}})];
    let mut patches = Vec::new();
    for series in 0..8 {
        let data = vec![0; 500];
        put.push(serde_json::json!({"series":{"id":format!("s{series}"),"in":"c","data":data}}));
        patches.push(serde_json::json!({"target":format!("s{series}"),"data":data}));
    }
    let mut extra = vec![Arc::new(
        serde_json::from_value(serde_json::json!({"id":"dense","put":put}))
            .expect("dense recovery step"),
    )];
    for index in 1..40 {
        extra.push(Arc::new(
            serde_json::from_value(
                serde_json::json!({"id":format!("update{index}"),"set":patches}),
            )
            .expect("dense recovery update"),
        ));
    }
    let (mut brain, scene) = typed_brain_with_steps(&scratch, None, &extra);
    let snapshot = brain
        .coordinator
        .drawing_snapshot()
        .expect("valid large recovery");
    let bytes: usize = snapshot
        .chunks
        .iter()
        .map(|chunk| {
            frame::encode(
                &DataToShell::SceneSnapshotV1 {
                    chunk: chunk.clone(),
                },
                Pipe::Data,
            )
            .expect("encoded recovery chunk")
            .len()
        })
        .sum();
    assert!(bytes > 2 * super::super::DATA_CAP);
    brain.crashes.plan(std::time::Instant::now());
    let ((control, data), mut shell) = super::duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let Some(ControlToShell::ScenesV1 { scenes }) = shell.control().await else {
            panic!("typed membership")
        };
        assert_eq!(
            scenes.iter().map(|named| named.scene).collect::<Vec<_>>(),
            [scene]
        );
        let DataToShell::SceneSnapshotV1 { chunk } = shell.data().await else {
            panic!("first typed recovery chunk")
        };
        assert_eq!(chunk.scene, scene);
        assert!(!chunk.last);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, super::super::Ended::Lost);
    let late = std::time::Instant::now() + std::time::Duration::from_secs(11);
    let plan = brain.crashes.plan(late);
    assert_eq!(plan.delay, Some(std::time::Duration::from_secs(1)));
    assert_eq!(plan.clear, [scene]);
    brain.close().await;
}

#[tokio::test]
async fn crash_clear_confirms_launch_only_after_commit_and_stays_clear_on_reopen() {
    let scratch = Scratch::new("typed-crash-clear");
    let (mut brain, scene) = typed_brain(&scratch, None);
    let now = std::time::Instant::now();
    brain.crashes.plan(now);
    brain.crashes.described(vec![scene], now);
    let plan = brain.crashes.plan(now);
    assert_eq!(plan.delay, Some(std::time::Duration::from_secs(1)));
    assert_eq!(plan.clear, [scene]);
    let authorized = {
        let mut observed = brain.observe();
        let release =
            brain.hold_store(|op| matches!(op, crate::coordinator::StoreOp::ClearScenes { .. }));
        let clearing = brain.clear_before_relaunch(&plan.clear);
        tokio::pin!(clearing);
        loop {
            tokio::select! {
                result = &mut clearing => panic!("launch authorized before commit: {result}"),
                seen = observed.recv() => {
                    if matches!(seen.expect("store observation"), super::super::Seen::StoreOpStarted(crate::coordinator::StoreOp::ClearScenes { .. })) {
                        break;
                    }
                }
            }
        }
        release.send(()).expect("release clear");
        clearing.await
    };
    assert!(authorized);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(store.drawing_scenes().expect("durable clear").scenes, []);
    let history: (u32, u32, u32) = store
        .conn_for_tests()
        .query_row(
            "SELECT sum(kind='scene_begin'),sum(kind='scene_step'),sum(kind='scene_cleared') FROM events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("typed history remains");
    assert_eq!(history, (1, 2, 1));
}

#[tokio::test]
async fn failed_crash_clear_cannot_authorize_launch_or_forget_its_debt() {
    let scratch = Scratch::new("typed-crash-clear-failure");
    let (mut brain, scene) = typed_brain(
        &scratch,
        Some(
            "CREATE TRIGGER fail_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' BEGIN SELECT RAISE(FAIL,'private drawing'); END;",
        ),
    );
    assert!(!brain.clear_before_relaunch(&[scene]).await);
    assert!(!brain.coordinator.settled());
    assert!(!brain.clear_before_relaunch(&[]).await);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(
        store
            .drawing_scenes()
            .expect("uncleared scene")
            .scenes
            .iter()
            .map(|scene| scene.scene)
            .collect::<Vec<_>>(),
        [scene]
    );
}

#[tokio::test]
async fn timed_out_crash_clear_cannot_authorize_launch() {
    let scratch = Scratch::new("typed-crash-clear-timeout");
    let (mut brain, scene) = typed_brain(&scratch, None);
    let release =
        brain.hold_store(|op| matches!(op, crate::coordinator::StoreOp::ClearScenes { .. }));
    super::product_waits();
    assert!(!brain.clear_before_relaunch(&[scene]).await);
    assert!(!brain.coordinator.settled());
    release.send(()).expect("release timed out clear");
    let reply = tokio::time::timeout(super::PATIENCE, brain.inputs.recv())
        .await
        .expect("eventual commit deadline")
        .expect("clear reply");
    assert!(
        matches!(&reply, crate::coordinator::Input::ScenesCleared { scenes, ok: true } if scenes == &[scene])
    );
    brain.offline(reply);
    brain.close().await;
}

#[tokio::test(start_paused = true)]
async fn reaccount_receipts_are_accepted_before_but_not_at_the_barrier_deadline() {
    use super::super::recovery::{Reaccount, reaccounted};
    use tokio::time::Instant;

    for at_deadline in [false, true] {
        let scratch = Scratch::new("startup-reaccount-result");
        let (mut brain, _) = typed_brain(&scratch, None);
        let metrics = Some(reaccount_profile());
        let job = brain
            .coordinator
            .reaccount_drawings(brain.epoch, metrics)
            .expect("changed measure");
        brain.drawing_reaccount = Some(Reaccount::start(job));
        let receipt = reaccounted(&mut brain.drawing_reaccount)
            .await
            .expect("not cancelled");
        assert!(receipt.result.is_some(), "the worker recovered the scene");
        let deadline = Instant::now() + super::PATIENCE;
        if at_deadline {
            tokio::time::advance(super::PATIENCE).await;
            assert_eq!(Instant::now(), deadline);
        }
        brain.receive_reaccount(deadline, receipt);
        assert_eq!(
            brain
                .coordinator
                .reaccount_drawings(brain.epoch, metrics)
                .is_some(),
            at_deadline,
            "only the before-deadline receipt installs the new measure"
        );
        brain.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn optional_reaccount_timeout_serves_the_settled_scene_and_fences_its_late_result() {
    use super::super::recovery::{Barrier, Reaccount, reaccounted};
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    let scratch = Scratch::new("startup-reaccount-timeout");
    let (mut brain, scene) = typed_brain(&scratch, None);
    let metrics = Some(reaccount_profile());
    let job = brain
        .coordinator
        .reaccount_drawings(brain.epoch, metrics)
        .expect("changed measure");
    let (started, running) = oneshot::channel();
    let (release, held) = oneshot::channel();
    brain.drawing_reaccount = Some(Reaccount::spawn(job, move |job, cancel| {
        let result = job.run(cancel).expect("worker recovers the scene");
        started.send(()).expect("test waits on worker");
        held.blocking_recv().ok()?;
        Some(result)
    }));
    running.await.expect("blocking worker started");

    let (out, mut controls) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.text_metrics = metrics;
    let mut barrier = Barrier {
        deadline: Instant::now() + super::PATIENCE,
        settled: false,
    };
    brain
        .advance_scene_barrier(&mut link, &mut barrier)
        .unwrap();
    assert!(barrier.settled);
    assert_eq!(controls.try_recv(), Err(mpsc::error::TryRecvError::Empty));

    tokio::time::advance(super::PATIENCE).await;
    brain
        .advance_scene_barrier(&mut link, &mut barrier)
        .unwrap();
    let control = controls
        .try_recv()
        .expect("settled membership without reconnect");
    let message = frame::read::<ControlToShell>(&mut control.as_slice(), Pipe::Control)
        .unwrap()
        .unwrap();
    assert!(matches!(message, ControlToShell::ScenesV1 { scenes }
        if scenes.len() == 1 && scenes[0].scene == scene && scenes[0].revision == 2));
    let snapshot = link.recovery.as_mut().unwrap().next_chunk().unwrap();
    assert!(matches!(snapshot, DataToShell::SceneSnapshotV1 { chunk }
        if chunk.scene == scene && chunk.events.len() == 2 && chunk.last));
    assert!(
        brain
            .coordinator
            .reaccount_drawings(brain.epoch, metrics)
            .is_none(),
        "the current measure is active even without paint accounting"
    );
    release.send(()).expect("blocking worker is still alive");
    assert!(reaccounted(&mut brain.drawing_reaccount).await.is_none());
    assert!(brain.drawing_reaccount.is_none(), "one worker was drained");
    brain.close().await;
}

#[tokio::test(start_paused = true)]
async fn a_required_clear_receipt_at_the_deadline_cannot_open_the_scene_barrier() {
    use super::super::{Ended, recovery::Barrier};
    use tokio::time::Instant;

    let scratch = Scratch::new("startup-required-clear");
    let (mut brain, scene) = typed_brain(&scratch, None);
    let release =
        brain.hold_store(|op| matches!(op, crate::coordinator::StoreOp::ClearScenes { .. }));
    let clear = brain.coordinator.failed_drawings(vec![scene]);
    brain.apply(clear, None).unwrap();
    let (out, mut controls) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.text_metrics = Some(reaccount_profile());
    let mut barrier = Barrier {
        deadline: Instant::now() + super::PATIENCE,
        settled: false,
    };
    brain
        .advance_scene_barrier(&mut link, &mut barrier)
        .unwrap();
    assert!(!barrier.settled);
    release.send(()).unwrap();
    let reply = brain.inputs.recv().await.expect("required clear receipt");
    assert!(
        matches!(&reply, crate::coordinator::Input::ScenesCleared { scenes, ok: true } if scenes == &[scene])
    );
    tokio::time::advance(super::PATIENCE).await;
    brain.offline(reply);
    assert!(brain.coordinator.settled());
    assert_eq!(
        brain.advance_scene_barrier(&mut link, &mut barrier),
        Err(Ended::Unavailable)
    );
    assert_eq!(controls.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain.close().await;
}

fn reaccount_profile() -> kumochi_scene::display::TextMetricsV1 {
    kumochi_scene::display::TextMetricsV1 {
        advances: [2048; 95],
        line_height: 1536,
        range_maxima: [1024, 1536, 2048],
    }
}

#[tokio::test]
async fn missing_store_cannot_authorize_crash_relaunch() {
    let scratch = Scratch::new("typed-crash-clear-store-gone");
    let (mut brain, scene) = typed_brain(&scratch, None);
    brain
        .store
        .take()
        .expect("store thread")
        .close()
        .await
        .expect("close");
    assert!(!brain.clear_before_relaunch(&[scene]).await);
    brain.close().await;
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
    let quit = brain.quit(tokio::time::Instant::now() + super::PATIENCE);
    let released = async {
        while !matches!(
            observed.recv().await.expect("quit observation"),
            super::super::Seen::QuitDraining
        ) {}
        release.send(()).expect("release cut");
    };
    let (quit, ()) = tokio::join!(quit, released);
    assert_eq!(quit.quit, Some(super::super::Quit::Complete));
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
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"bad","surface":"overlay","alt":"diagram"},"layout_version":2})).expect("header");
    let step = Arc::new(serde_json::from_value(serde_json::json!({"id":"a","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]})).expect("step"));
    let Reply::Step(bad) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step,
            protected: false,
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
    *brain.coordinator =
        crate::coordinator::Coordinator::ready_for_tests(main, last_thread, last_run, Vec::new());
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
    let reply = tokio::time::timeout(super::PATIENCE, brain.inputs.recv())
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
    let custody = crate::test_support::custody(&store);
    let mut brain = Brain::new(
        store,
        custody,
        main,
        high,
        Vec::new(),
        crate::runtime::Selected::Fixture(Box::new(Fixture::parse("").expect("fixture"))),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("a failed drawing query must not stop the brain");
    assert!(!brain.coordinator.drawing_available());
    assert!(!brain.clear_before_relaunch(&[]).await);
    let repair = rusqlite::Connection::open(scratch.db()).expect("repair connection");
    repair
        .execute_batch("ALTER TABLE unreadable_threads RENAME TO threads")
        .expect("restore reads");
    drop(repair);
    assert_eq!(
        brain
            .quit(tokio::time::Instant::now() + super::PATIENCE)
            .await
            .quit,
        Some(super::super::Quit::Incomplete)
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
    let failed = tokio::time::timeout(super::PATIENCE, brain.inputs.recv())
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
            .quit(tokio::time::Instant::now() + super::PATIENCE)
            .await
            .quit,
        Some(super::super::Quit::Complete)
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
            .quit(tokio::time::Instant::now() + super::PATIENCE)
            .await
            .quit,
        Some(super::super::Quit::Incomplete)
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
#[expect(
    clippy::too_many_lines,
    reason = "one restart, from the first run to the queued log"
)]
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
        "layout_version":2
    }))
    .expect("typed header");
    let step = stored_step("first", "box");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step: Arc::clone(&step),
            protected: false,
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
    let custody = crate::test_support::custody(&store);
    let mut brain = Brain::new(
        store,
        custody,
        main,
        high,
        Vec::new(),
        crate::runtime::Selected::Fixture(Box::new(Fixture::parse("").expect("fixture"))),
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
                    speech: None,
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
