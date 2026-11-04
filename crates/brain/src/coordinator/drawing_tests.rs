use kumochi_protocol::{
    frame::{self, Pipe},
    message::{ControlToShell, DataToShell, Phase, SceneCuts, ThreadId},
};
use serde_json::json;

use super::{Coordinator, Effect, Input, R, StoreOp, T, started};
use crate::{
    drawing::{self, Write},
    engines::{CallId, Finish, TurnEvent},
    store::{RunEnd, Store},
    test_support::Scratch,
};

fn open(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store.main_thread().expect("main");
    store.begin_turn(T, Some("test"), R, "turn").expect("run");
    store
}

fn draw(core: &mut Coordinator) -> Vec<Effect> {
    draw_named(core, "draw", "diagram")
}

fn draw_named(core: &mut Coordinator, call: &str, scene: &str) -> Vec<Effect> {
    let args = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":scene,"surface":"overlay","alt":"test diagram"}},
        {"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}},
        {"step":{"id":"s2","put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("arguments");
    core.handle(Input::Drawing(drawing::Input::Call {
        thread: T,
        run: R,
        call: CallId::new(call).expect("call"),
        args,
    }))
    .expect("fresh call")
}

fn pending(effects: &[Effect]) -> (u64, Write) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
            _ => None,
        })
        .expect("one drawing write")
}

fn commit(core: &mut Coordinator, store: &mut Store, effects: &[Effect]) -> Vec<Effect> {
    let (serial, write) = pending(effects);
    let reply = store.drawing_write(&write).expect("transaction");
    core.handle(Input::DrawingWritten {
        serial,
        reply: Ok(reply),
    })
    .expect("matching acknowledgement")
}

#[test]
fn provider_completion_waits_for_the_accepted_drawing_prefix() {
    let scratch = Scratch::new("drawing-completion");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let completed = core
        .handle(Input::Engine {
            run: R,
            event: TurnEvent::Finished(Finish::Completed),
        })
        .expect("provider finished");
    assert!(!completed.iter().any(|e| matches!(
        e,
        Effect::Store(StoreOp::EndRun { .. }) | Effect::Data(DataToShell::RunEnd { .. })
    )));
    let second = commit(&mut core, &mut store, &first);
    assert!(
        !second
            .iter()
            .any(|e| matches!(e, Effect::Store(StoreOp::EndRun { .. })))
    );
    let final_step = commit(&mut core, &mut store, &second);
    assert!(final_step.iter().any(|e| matches!(
        e,
        Effect::Data(DataToShell::SceneStepCommit { index: 1, .. })
    )));
    assert!(final_step.iter().any(|e| matches!(
        e,
        Effect::Store(StoreOp::EndRun {
            end: RunEnd::Done,
            ..
        })
    )));
    assert_eq!(
        store.drawing_scenes().expect("history").scenes[0].log.len(),
        2
    );
    let calls = core.take_drawing_results();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].complete());
    assert_eq!(
        calls[0]
            .drawn
            .iter()
            .map(kumochi_scene::WireId::as_str)
            .collect::<Vec<_>>(),
        ["s1", "s2"]
    );
    store.end_run(T, R, &[], RunEnd::Done).expect("outcome");
    let end = core
        .handle(Input::RunStored { run: R })
        .expect("stored outcome");
    assert!(end.contains(&Effect::Control(ControlToShell::RunState {
        run: R,
        thread: T,
        phase: Phase::Done,
        error: None,
    })));
    assert!(end.contains(&Effect::Data(DataToShell::RunEnd { run: R })));
}

#[test]
fn a_legacy_ack_cannot_finish_a_run_whose_typed_prefix_is_still_being_stored() {
    let scratch = Scratch::new("drawing-mixed-completion");
    let mut store = open(&scratch);
    let mut core = started();
    super::draw_step(&mut core, R, "legacy").expect("legacy step accepted");
    let legacy = store
        .commit_step(
            (T, R),
            None,
            crate::store::SceneTarget::New { evict: None },
            0,
            "legacy",
        )
        .expect("legacy transaction");
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    core.handle(Input::Engine {
        run: R,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .expect("provider done");
    let acked = core
        .handle(Input::StepStored {
            run: R,
            index: 0,
            step: super::step("legacy"),
            scene: legacy,
        })
        .expect("legacy ack");
    assert!(
        !acked
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
    );
    assert_eq!(core.current_run(), Some(R));
    let second = commit(&mut core, &mut store, &first);
    let final_step = commit(&mut core, &mut store, &second);
    assert!(final_step.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::EndRun {
            end: RunEnd::Done,
            ..
        })
    )));
}

#[test]
fn stop_before_a_scene_id_arrives_drains_then_durably_cuts_the_first_step() {
    let scratch = Scratch::new("drawing-stop-first-write");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let stopped = core
        .handle(Input::StopScenes {
            run: R,
            cuts: SceneCuts::default(),
        })
        .expect("stop");
    assert!(stopped.contains(&Effect::Control(ControlToShell::RunState {
        run: R,
        thread: T,
        phase: Phase::Interrupted,
        error: None,
    })));
    assert!(!stopped.contains(&Effect::Data(DataToShell::RunEnd { run: R })));
    let stored = commit(&mut core, &mut store, &first);
    let step = stored
        .iter()
        .position(|e| {
            matches!(
                e,
                Effect::Data(DataToShell::SceneStepCommit { index: 0, .. })
            )
        })
        .expect("durable first step delivered");
    let end = stored
        .iter()
        .position(|e| matches!(e, Effect::Data(DataToShell::RunEnd { .. })))
        .expect("run data closes after step");
    assert!(step < end);
    let (_, cut) = pending(&stored);
    assert!(matches!(
        cut,
        Write::Cut {
            cut: kumochi_scene::StopCut { activated: 0, .. },
            ..
        }
    ));
    let cut_stored = commit(&mut core, &mut store, &stored);
    assert!(cut_stored.iter().any(|e| matches!(
        e,
        Effect::Data(DataToShell::SceneCutApplied {
            cut: kumochi_scene::StopCut { activated: 0, .. },
            ..
        })
    )));
    assert!(
        store
            .drawing_scenes()
            .expect("cut persists even before the clear write")
            .scenes
            .is_empty()
    );
    for effect in cut_stored {
        if let Effect::Store(StoreOp::ClearScenes { scenes }) = effect {
            store.clear_scenes(&scenes).expect("clear");
            core.handle(Input::ScenesCleared { scenes, ok: true })
                .expect("clear acknowledgement");
        }
    }
    assert!(core.drawing_snapshot().expect("snapshot").scenes.is_empty());
}

#[test]
fn real_store_recovery_produces_decodable_descriptors_and_chunks() {
    let scratch = Scratch::new("drawing-snapshot-wire");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let mut recovered = Coordinator::ready_for_tests(ThreadId(1), T, R, Vec::new());
    assert!(
        recovered
            .restore_drawings(store.drawing_scenes())
            .is_empty()
    );
    let snapshot = recovered.drawing_snapshot().expect("producer validates");
    assert_eq!(snapshot.scenes.len(), 1);
    assert_eq!(snapshot.scenes[0].revision, 2);
    assert_eq!(snapshot.scenes[0].committed, 2);
    assert_eq!(snapshot.chunks.len(), 1);
    assert_eq!(snapshot.chunks[0].events.len(), 2);
    assert!(snapshot.chunks[0].last);
    assert_eq!(
        snapshot.chunks[0]
            .header
            .as_ref()
            .expect("chunk zero header")
            .begin
            .alt
            .as_str(),
        "test diagram"
    );
    let membership = ControlToShell::ScenesV1 {
        scenes: snapshot.scenes,
    };
    let bytes = frame::encode(&membership, Pipe::Control).expect("membership frame");
    assert_eq!(
        frame::read::<ControlToShell>(&mut bytes.as_slice(), Pipe::Control).expect("decode"),
        Some(membership)
    );
    for chunk in snapshot.chunks {
        let message = DataToShell::SceneSnapshotV1 { chunk };
        let bytes = frame::encode(&message, Pipe::Data).expect("whole-event frame");
        assert_eq!(
            frame::read::<DataToShell>(&mut bytes.as_slice(), Pipe::Data).expect("decode"),
            Some(message)
        );
    }
    let bad = core
        .handle(Input::Drawing(drawing::Input::StepShown {
            run: R,
            scene: store.drawing_scenes().expect("scene").scenes[0].scene,
            index: u32::MAX,
        }))
        .expect("bad notification classified");
    assert!(bad.iter().any(|e| matches!(e, Effect::Broken(_))));
    assert_eq!(
        store.drawing_scenes().expect("unchanged log").scenes[0]
            .log
            .len(),
        2
    );
}

fn deletion_before_ack() -> Coordinator {
    let scratch = Scratch::new("drawing-delete-held");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    assert!(core.drawing_snapshot().is_err());
    core.handle(Input::Delete {
        thread: T,
        after_turns: 0,
        resend: false,
    })
    .expect("accepted delete");
    let stored = commit(&mut core, &mut store, &first);
    assert!(!stored.iter().any(|effect| matches!(
        effect,
        Effect::Data(DataToShell::SceneBegin { .. } | DataToShell::SceneStepCommit { .. })
    )));
    core
}

fn deletion_during_write(
    outcome: kumochi_protocol::message::Deleted,
) -> (Coordinator, Vec<Effect>) {
    let mut core = deletion_before_ack();
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome,
            last_run: Some(R),
        })
        .expect("delete result");
    (core, answered)
}

#[test]
fn reconnect_snapshot_supersedes_frames_held_for_a_pending_delete() {
    let mut core = deletion_before_ack();
    let snapshot = core
        .drawing_snapshot()
        .expect("the committed scene is recoverable");
    assert_eq!(snapshot.scenes[0].committed, 1);
    core.drawing_described();
    let effects = core
        .handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete");
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Data(DataToShell::SceneBegin { .. } | DataToShell::SceneStepCommit { .. })
    )));
    assert_eq!(
        core.drawing_snapshot()
            .expect("canonical scene remains")
            .scenes,
        snapshot.scenes
    );
}

#[test]
fn coordinator_debug_does_not_format_frames_held_for_deletion() {
    let core = deletion_before_ack();
    assert_eq!(core.drawing_held.len(), 2);
    let debug = format!("{core:?}");
    assert!(debug.contains("drawing_held: 2"));
    assert!(!debug.contains("SceneBegin"));
    assert!(!debug.contains("SceneStepCommit"));
}

#[test]
fn a_refused_delete_releases_late_drawing_frames_without_losing_canonical_state() {
    let (core, effects) = deletion_during_write(kumochi_protocol::message::Deleted::NotHidden);
    let snapshot = core.drawing_snapshot().expect("settled scene");
    assert_eq!(snapshot.scenes.len(), 1);
    assert_eq!(snapshot.scenes[0].committed, 1);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Data(DataToShell::SceneBegin { .. })))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Data(DataToShell::SceneStepCommit { index: 0, .. })
            ))
            .count(),
        1
    );
}

#[test]
fn a_hidden_thread_never_releases_late_drawing_frames() {
    let (core, effects) = deletion_during_write(kumochi_protocol::message::Deleted::Hidden);
    assert!(
        core.drawing_snapshot()
            .expect("retired scene")
            .scenes
            .is_empty()
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Data(DataToShell::SceneBegin { .. } | DataToShell::SceneStepCommit { .. })
    )));
}

#[test]
fn a_stop_naming_an_unissued_run_cannot_cut_a_held_scene() {
    let scratch = Scratch::new("drawing-future-stop");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let snapshot = core.drawing_snapshot().expect("settled drawing");
    let cuts = SceneCuts::new(vec![kumochi_protocol::message::SceneCut {
        scene: snapshot.scenes[0].scene,
        cut: serde_json::from_str(r#"{"activated":0,"revealed":[]}"#).expect("zero cut"),
    }])
    .expect("cut list");
    assert_eq!(
        core.handle(Input::StopScenes {
            run: kumochi_protocol::message::RunId(R.0 + 1),
            cuts
        }),
        Err(super::Stale)
    );
    assert_eq!(core.current_run(), Some(R));
    assert_eq!(
        core.drawing_snapshot().expect("unchanged scene").scenes,
        snapshot.scenes
    );
}

fn failed_cut(core: &mut Coordinator, scene: kumochi_protocol::message::SceneId) -> Vec<Effect> {
    let cuts = SceneCuts::new(vec![kumochi_protocol::message::SceneCut {
        scene,
        cut: kumochi_scene::StopCut {
            activated: 1,
            revealed: kumochi_scene::text::Bounded::new(vec![
                kumochi_scene::Handle::new(1, 0).expect("handle"),
            ])
            .expect("one handle"),
        },
    }])
    .expect("one cut");
    let cutting = core
        .handle(Input::StopScenes { run: R, cuts })
        .expect("late cut");
    let (serial, _) = pending(&cutting);
    core.handle(Input::DrawingWritten {
        serial,
        reply: Err(()),
    })
    .expect("failed cut")
}

#[test]
fn clearing_cut_debt_finishes_a_provider_completed_run_without_another_drawing_ack() {
    let scratch = Scratch::new("drawing-clear-completion");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    core.handle(Input::Engine {
        run: R,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .expect("first run finished");
    store
        .end_run(T, R, &[], RunEnd::Done)
        .expect("first outcome");
    core.handle(Input::RunStored { run: R })
        .expect("first outcome ack");
    let next = kumochi_protocol::message::RunId(11);
    core.handle(Input::Turn {
        thread: Some(T),
        text: "continue".to_owned(),
    })
    .expect("next turn");
    store
        .begin_turn(T, None, next, "continue")
        .expect("next turn stored");
    super::admit(&mut core, next);
    let call = CallId::new("continuing").expect("call");
    let record = |index, bytes: &[u8]| {
        Input::Drawing(drawing::Input::Record {
            thread: T,
            run: next,
            call: call.clone(),
            index,
            bytes: bytes.to_vec(),
        })
    };
    core.handle(record(
        0,
        br#"{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram"}}"#,
    ))
    .expect("begin");
    let scene = store.drawing_scenes().expect("scene").scenes[0].scene;
    failed_cut(&mut core, scene);
    core.handle(record(
        1,
        br#"{"step":{"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[40,0,10,10]}}]}}"#,
    ))
    .expect("queued step");
    core.handle(record(2, br#"{"end":{}}"#))
        .expect("queued end");
    core.handle(Input::Drawing(drawing::Input::CallFinished {
        thread: T,
        run: next,
        call,
        error: None,
    }))
    .expect("queued finish");
    core.handle(Input::Engine {
        run: next,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .expect("provider complete");
    let effects = core
        .handle(Input::Clear {
            scenes: vec![scene],
        })
        .expect("clear supersedes debt");
    assert!(effects.iter().any(|effect| matches!(effect,Effect::Store(StoreOp::EndRun {run,end:RunEnd::Done,..}) if *run==next)));
    assert_eq!(core.current_run(), None);
    store.clear_scenes(&[scene]).expect("clear stored");
    core.handle(Input::ScenesCleared {
        scenes: vec![scene],
        ok: true,
    })
    .expect("clear ack");
    store
        .end_run(T, next, &[], RunEnd::Done)
        .expect("next outcome");
    let done = core
        .handle(Input::RunStored { run: next })
        .expect("outcome ack");
    assert!(done.contains(&Effect::Control(ControlToShell::RunState {
        run: next,
        thread: T,
        phase: Phase::Done,
        error: None,
    })));
}

#[test]
fn repeated_cut_failures_back_off_until_the_existing_thirty_second_cap() {
    let scratch = Scratch::new("drawing-cut-backoff");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let scene = store.drawing_scenes().expect("scene").scenes[0].scene;
    let failed = failed_cut(&mut core, scene);
    assert!(failed.contains(&Effect::ArmRetry {
        after: std::time::Duration::from_millis(500)
    }));
    for millis in [1000, 2000, 4000, 8000, 16000, 30000, 30000] {
        let retry = core.handle(Input::Retry).expect("retry timer");
        let (serial, write) = pending(&retry);
        assert!(matches!(write, Write::Cut { .. }));
        let failed = core
            .handle(Input::DrawingWritten {
                serial,
                reply: Err(()),
            })
            .expect("failed retry");
        let waits: Vec<_> = failed
            .iter()
            .filter_map(|effect| match effect {
                Effect::ArmRetry { after } => Some(*after),
                _ => None,
            })
            .collect();
        assert_eq!(waits, [std::time::Duration::from_millis(millis)]);
    }
}

fn four_drawings(
    core: &mut Coordinator,
    store: &mut Store,
) -> Vec<kumochi_protocol::message::SceneId> {
    for index in 1..=4 {
        let lookup = draw_named(core, &format!("call-{index}"), &format!("scene-{index}"));
        let first = commit(core, store, &lookup);
        let second = commit(core, store, &first);
        commit(core, store, &second);
    }
    store
        .drawing_scenes()
        .expect("original scenes")
        .scenes
        .iter()
        .map(|scene| scene.scene)
        .collect()
}

fn clear_answer(
    core: &mut Coordinator,
    store: &mut Store,
    scene: kumochi_protocol::message::SceneId,
) -> Vec<Effect> {
    let ok = store.clear_scenes(&[scene]).is_ok();
    core.handle(Input::ScenesCleared {
        scenes: vec![scene],
        ok,
    })
    .expect("clear reply")
}

fn recovered_ids(store: &Store) -> Vec<kumochi_protocol::message::SceneId> {
    store
        .drawing_scenes()
        .expect("recovered membership")
        .scenes
        .iter()
        .map(|scene| scene.scene)
        .collect()
}

#[test]
fn a_failed_eviction_clear_blocks_the_new_scene_then_a_failed_step_cannot_resurrect_it() {
    let scratch = Scratch::new("drawing-eviction-retry");
    let mut store = open(&scratch);
    let mut core = started();
    let original = four_drawings(&mut core, &mut store);
    assert_eq!(original.len(), 4);
    store.conn_for_tests().execute_batch("CREATE TRIGGER fail_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' BEGIN SELECT RAISE(FAIL,'private drawing'); END;").expect("fail clear");
    let lookup = draw_named(&mut core, "fifth", "scene-5");
    let first = commit(&mut core, &mut store, &lookup);
    assert!(first.contains(&Effect::Store(StoreOp::ClearScenes {
        scenes: vec![original[0]]
    })));
    assert!(!first.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::Drawing {
            write: Write::Step { .. },
            ..
        })
    )));
    assert!(
        !clear_answer(&mut core, &mut store, original[0])
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. })))
    );
    assert_eq!(core.debt(), 1);
    assert_eq!(recovered_ids(&store), original);
    store
        .conn_for_tests()
        .execute_batch("DROP TRIGGER fail_clear;")
        .expect("allow clear retry");
    let retried = core.handle(Input::Retry).expect("retry");
    assert!(retried.contains(&Effect::Store(StoreOp::ClearScenes {
        scenes: vec![original[0]]
    })));
    let first = clear_answer(&mut core, &mut store, original[0]);
    let (serial, write) = pending(&first);
    assert!(matches!(
        write,
        Write::Step {
            target: drawing::Target::New(_),
            ..
        }
    ));
    store.conn_for_tests().execute_batch("CREATE TRIGGER fail_step BEFORE INSERT ON events WHEN new.kind='scene_step' BEGIN SELECT RAISE(FAIL,'private drawing'); END;").expect("fail new step");
    assert!(store.drawing_write(&write).is_err());
    core.handle(Input::DrawingWritten {
        serial,
        reply: Err(()),
    })
    .expect("failed new step");
    assert_eq!(core.debt(), 0);
    assert_eq!(recovered_ids(&store), original[1..]);
    store
        .conn_for_tests()
        .execute_batch("DROP TRIGGER fail_step;")
        .expect("allow new step retry");
    let lookup = draw_named(&mut core, "retry-fifth", "scene-5");
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let after = recovered_ids(&store);
    assert_eq!(&after[..3], &original[1..]);
    assert_eq!(after.len(), 4);
    core.handle(Input::Clear {
        scenes: vec![original[1]],
    })
    .expect("clear another scene");
    clear_answer(&mut core, &mut store, original[1]);
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(recovered_ids(&store), after[1..]);
}

#[test]
fn a_failed_user_clear_cannot_let_a_new_scene_displace_an_uncleared_visible_scene() {
    let scratch = Scratch::new("drawing-user-clear-retry");
    let mut store = open(&scratch);
    let mut core = started();
    let original = four_drawings(&mut core, &mut store);
    store.conn_for_tests().execute_batch("CREATE TRIGGER fail_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' BEGIN SELECT RAISE(FAIL,'private drawing'); END;").expect("fail user clear");
    core.handle(Input::Clear {
        scenes: vec![original[2]],
    })
    .expect("user clear");
    clear_answer(&mut core, &mut store, original[2]);
    let lookup = draw_named(&mut core, "fifth", "scene-5");
    assert!(lookup.contains(&Effect::Store(StoreOp::ClearScenes {
        scenes: vec![original[2]],
    })));
    let first = commit(&mut core, &mut store, &lookup);
    clear_answer(&mut core, &mut store, original[2]);
    assert!(!first.iter().any(|effect| matches!(
        effect,
        Effect::Store(StoreOp::Drawing {
            write: Write::Step { .. },
            ..
        })
    )));
    assert_eq!(core.debt(), 1);
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen while the clear is owed");
    assert_eq!(recovered_ids(&store), original);
    store
        .conn_for_tests()
        .execute_batch("DROP TRIGGER fail_clear;")
        .expect("allow clear retry");
    let retry = core.handle(Input::Retry).expect("retry user clear");
    assert!(retry.contains(&Effect::Store(StoreOp::ClearScenes {
        scenes: vec![original[2]]
    })));
    let first = clear_answer(&mut core, &mut store, original[2]);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let after = recovered_ids(&store);
    assert_eq!(after.len(), 4);
    assert_eq!(&after[..3], &[original[0], original[1], original[3]]);
    drop(store);
    let store = Store::open(&scratch.db()).expect("reopen after the clear and new commit");
    assert_eq!(recovered_ids(&store), after);
}

#[test]
fn a_bad_stored_row_cannot_wedge_a_valid_live_append_or_cut() {
    let scratch = Scratch::new("drawing-no-store-replay");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    commit(&mut core, &mut store, &second);
    let scene = recovered_ids(&store)[0];
    store.conn_for_tests().execute_batch("DROP TRIGGER events_append_only; UPDATE events SET body='{' WHERE id=(SELECT min(id) FROM events WHERE kind='scene_step');").expect("corrupt stored history after canonical admission");
    let args = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram"}},
        {"step":{"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[40,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("arguments");
    let written = core
        .handle(Input::Drawing(drawing::Input::Call {
            thread: T,
            run: R,
            call: CallId::new("continuing").expect("call"),
            args,
        }))
        .expect("canonical live append");
    let committed = commit(&mut core, &mut store, &written);
    assert!(committed.iter().any(|effect| matches!(
        effect,
        Effect::Data(DataToShell::SceneStepCommit { index: 2, .. })
    )));
    let cut = kumochi_scene::StopCut {
        activated: 1,
        revealed: kumochi_scene::text::Bounded::new(vec![
            kumochi_scene::Handle::new(1, 0).expect("handle"),
        ])
        .expect("handles"),
    };
    let cutting = core
        .handle(Input::StopScenes {
            run: R,
            cuts: SceneCuts::new(vec![kumochi_protocol::message::SceneCut {
                scene,
                cut: cut.clone(),
            }])
            .expect("one cut"),
        })
        .expect("canonical cut");
    let committed = commit(&mut core, &mut store, &cutting);
    assert!(committed.iter().any(|effect|matches!(effect,Effect::Data(DataToShell::SceneCutApplied {cut:applied,..}) if applied == &cut)));
    let recovery = store
        .drawing_scenes()
        .expect("a later restart isolates the bad scene");
    assert!(recovery.scenes.is_empty());
    assert_eq!(recovery.skipped, [scene]);
}
