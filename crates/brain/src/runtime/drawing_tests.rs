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
    let step = Arc::new(
        serde_json::from_value(serde_json::json!({
            "id":"first","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,10,10]}}]
        }))
        .expect("typed step"),
    );
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
    let mut recovery = brain.describe(&mut link).expect("validated recovery");
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
