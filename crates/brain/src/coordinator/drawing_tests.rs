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
    let args = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram"}},
        {"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}},
        {"step":{"id":"s2","put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("arguments");
    core.handle(Input::Drawing(drawing::Input::Call {
        thread: T,
        run: R,
        call: CallId::new("draw").expect("call"),
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
    assert_eq!(store.drawing_scenes().expect("history")[0].log.len(), 2);
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
        phase: Phase::Done
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
        phase: Phase::Interrupted
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
    let mut recovered = Coordinator::new(ThreadId(1), T, R, Vec::new());
    recovered
        .restore_drawings(store.drawing_scenes().expect("typed log"))
        .expect("replay");
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
            scene: store.drawing_scenes().expect("scene")[0].scene,
            index: u32::MAX,
        }))
        .expect("bad notification classified");
    assert!(bad.iter().any(|e| matches!(e, Effect::Broken(_))));
    assert_eq!(
        store.drawing_scenes().expect("unchanged log")[0].log.len(),
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
