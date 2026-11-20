use kumochi_protocol::{
    frame::{self, Pipe},
    message::{ControlToShell, DataToShell, Phase, SceneCuts, ThreadId},
};
use serde_json::json;

use super::super::Notify;
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

#[test]
fn sixty_five_fixture_calls_across_runs_retain_no_session_outcomes() {
    let mut core = super::fresh();
    for offset in 0..65 {
        let run = kumochi_protocol::message::RunId(R.0 + offset);
        core.handle(Input::Turn {
            turn: u32::try_from(offset + 1).expect("65 turns"),
            thread: (offset != 0).then_some(T),
            text: "hi".into(),
        })
        .expect("new turn");
        super::admit(&mut core, run);
        assert_eq!(
            core.handle(Input::Drawing(drawing::Input::Call {
                thread: T,
                run,
                call: CallId::new("fixture").expect("ASCII"),
                args: br#"{"records":[]}"#.to_vec(),
            })),
            Ok(Vec::new())
        );
        let effects = core
            .handle(Input::Engine {
                key: super::engine_key(&core, run),
                event: TurnEvent::Finished(Finish::Completed),
            })
            .expect("completed round");
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::Store(StoreOp::EndRun {
                        end: RunEnd::Done,
                        ..
                    })
                ))
                .count(),
            1
        );
        core.handle(Input::RunStored { run })
            .expect("stored outcome");
    }
}

fn draw(core: &mut Coordinator) -> Vec<Effect> {
    draw_named(core, "draw", "diagram")
}

fn draw_named(core: &mut Coordinator, call: &str, scene: &str) -> Vec<Effect> {
    let args = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":scene,"surface":"overlay","alt":"test diagram","others":"keep"}},
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
    let effects = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Ok(reply),
        })
        .expect("matching acknowledgement");
    super::prepared(core, effects)
}

/// A committed step's `say` is spoken as its own segment, named by its step, and its scene's
/// failure drops it.
#[test]
fn a_committed_step_s_say_is_spoken_and_its_failed_scene_drops_it() {
    use kumochi_protocol::message::{OutputGeneration, SegmentId, SpeechPcm, StepRef};
    let scratch = Scratch::new("drawing-say");
    let mut store = open(&scratch);
    let mut core = super::spoken();
    let args = serde_json::to_vec(&json!({"records":[
        {"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram"}},
        {"step":{"id":"s1","say":"Look here.","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}},
        {"end":{}}
    ]}))
    .expect("arguments");
    let mut lookup = Vec::new();
    let dispatched = core.drawing_call(
        T,
        &crate::dispatch::LedgerKey {
            run: R,
            route: crate::dispatch::Route::Model,
            call: "draw".into(),
        },
        crate::dispatch::DrawAction::Complete(crate::dispatch::validate_draw(&args)),
        &mut lookup,
    );
    assert_eq!(dispatched, crate::dispatch::Dispatch::Committing);
    let step_write = commit(&mut core, &mut store, &lookup);
    let first = commit(&mut core, &mut store, &step_write);
    let at = first
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::Data(DataToShell::SceneStepCommit { index: 0, .. })
            )
        })
        .expect("the step commits");
    let Effect::Data(DataToShell::SceneStepCommit { scene, .. }) = first[at] else {
        unreachable!("matched above");
    };
    let segment = SegmentId(0);
    assert_eq!(
        first[at + 1..at + 4],
        [
            Effect::Data(DataToShell::SegmentQueued {
                run: R,
                segment,
                step: Some(StepRef { scene, index: 0 }),
            }),
            Effect::Speak {
                run: R,
                segment,
                rate: 24_000,
                text: "Look here.".into(),
            },
            Effect::Credit {
                run: R,
                samples: 480_000,
            },
        ]
    );
    let failed = core
        .handle(Input::Drawing(drawing::Input::StepFailed {
            run: R,
            scene,
            index: 0,
            reason: kumochi_protocol::message::SceneFailure::Timeout,
            cut: kumochi_scene::StopCut {
                activated: 0,
                revealed: kumochi_scene::text::Bounded::new(Vec::new()).expect("none"),
            },
        }))
        .expect("a committed step");
    assert_eq!(
        failed[failed.len() - 2..],
        [
            Effect::CancelSpeech { run: R },
            Effect::Data(DataToShell::SpeechPcm(SpeechPcm {
                run: R,
                segment,
                output_generation: OutputGeneration(1),
                seq: 0,
                samples: Vec::new(),
                last: Some(0),
            })),
        ]
    );
}

/// A round that completes while its drawing is still being stored ends through the tool loop,
/// so its call is answered and its items stored before anything else.
#[test]
fn a_round_completed_behind_a_drawing_still_answers_its_calls() {
    let scratch = Scratch::new("drawing-round");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let call = CallId::new("tool-1").expect("call");
    for event in [
        TurnEvent::ToolCallStarted {
            call: call.clone(),
            name: crate::engines::ToolName::Unknown,
        },
        TurnEvent::ToolCallReady {
            call: call.clone(),
            args: "{}".to_owned(),
        },
        TurnEvent::Finished(Finish::Completed),
    ] {
        core.handle(Input::Engine {
            key: super::engine_key(&core, R),
            event,
        })
        .expect("current");
    }
    let first = commit(&mut core, &mut store, &lookup);
    let second = commit(&mut core, &mut store, &first);
    let last = commit(&mut core, &mut store, &second);
    let writes: Vec<_> = last
        .iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::RoundItems(round)) => {
                Some(round.calls.iter().map(|(call, _)| call.clone()).collect())
            }
            Effect::Store(StoreOp::EndRun { .. }) => Some(Vec::new()),
            _ => None,
        })
        .collect();
    assert_eq!(
        writes,
        [vec![call]],
        "the round's write, and no outcome yet"
    );
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
            key: super::engine_key(&core, R),
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
    assert_eq!(
        final_step
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Data(DataToShell::SceneEnd {
                    run: R,
                    complete: true,
                    ..
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        store.drawing_scenes().expect("history").scenes[0]
            .log
            .iter()
            .filter_map(|event| match event {
                kumochi_scene::SceneEvent::Step { step, .. } => Some(step.id.as_str()),
                kumochi_scene::SceneEvent::Stop(_) => None,
            })
            .collect::<Vec<_>>(),
        ["s1", "s2"]
    );
    store.end_run(T, R, &[], RunEnd::Done).expect("outcome");
    let end = core
        .handle(Input::RunStored { run: R })
        .expect("stored outcome");
    assert!(end.contains(&Effect::Control(ControlToShell::RunState {
        origin: None,
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
        key: super::engine_key(&core, R),
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
        origin: None,
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

fn deletion_before_ack(mut core: Coordinator) -> (Coordinator, Vec<Effect>) {
    let scratch = Scratch::new("drawing-delete-held");
    let mut store = open(&scratch);
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    assert!(core.drawing_snapshot().is_err());
    let mut effects = core
        .handle(Input::Delete {
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
    effects.extend(stored);
    (core, effects)
}

fn deletion_during_write(
    outcome: kumochi_protocol::message::Deleted,
) -> (Coordinator, Vec<Effect>) {
    let (mut core, mut effects) = deletion_before_ack(started());
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome,
            last_run: Some(R),
        })
        .expect("delete result");
    effects.extend(answered);
    (core, effects)
}

#[test]
fn reconnect_snapshot_supersedes_frames_held_for_a_pending_delete() {
    let (mut core, _) = deletion_before_ack(started());
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
        Effect::Data(
            DataToShell::SceneBegin { .. }
                | DataToShell::SceneStepCommit { .. }
                | DataToShell::RunEnd { .. }
        )
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
    let (core, _) = deletion_before_ack(started());
    assert_eq!(core.drawing_held.len(), 3);
    assert!(matches!(
        core.drawing_held[2].1,
        DataToShell::SceneEnd {
            complete: false,
            ..
        }
    ));
    let debug = format!("{core:?}");
    assert!(debug.contains("drawing_held: 3"));
    assert!(!debug.contains("SceneBegin"));
    assert!(!debug.contains("SceneStepCommit"));
    assert!(!debug.contains("SceneEnd"));
    assert!(!debug.contains("test diagram"));
    assert!(!debug.contains(&format!("{:?}", b"test diagram")));
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
    let ends: Vec<_> = effects
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| {
            matches!(effect, Effect::Data(DataToShell::RunEnd { run: R })).then_some(index)
        })
        .collect();
    assert_eq!(ends.len(), 1);
    let committed = effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::Data(DataToShell::SceneStepCommit { run: R, .. })
            )
        })
        .expect("replayed commit");
    assert!(
        committed < ends[0],
        "replay must precede the run's data end"
    );
    let closed = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Data(DataToShell::SceneEnd { run: R, .. })))
        .expect("replayed scene end");
    assert!(closed < ends[0]);
}

#[test]
fn deletion_held_data_end_does_not_hold_the_outcome_or_send_lease() {
    let mut core = started();
    let key = core
        .current
        .as_ref()
        .expect("current")
        .build
        .as_ref()
        .expect("build")
        .key();
    let granted = core
        .handle(Input::SendReady {
            key: super::super::SendKey::Build(key),
            hold: None,
        })
        .expect("send grant");
    let lease = granted
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted lease");
    let (mut core, mut effects) = deletion_before_ack(core);
    assert_eq!(core.held_deletes(), vec![T]);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { run: R, .. })))
    );
    effects.extend(
        core.handle(Input::RunStored { run: R })
            .expect("stored before delete"),
    );
    assert_eq!(core.ending_runs(), 0);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(DataToShell::RunEnd { run: R })))
    );
    let released = core
        .handle(Input::SendSettled { lease })
        .expect("settled send");
    assert_eq!(
        released
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::Delete { thread: T })))
            .count(),
        1
    );
    assert!(core.held_deletes().is_empty());
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete");
    assert_eq!(answered.last(), Some(&super::run_end(R)));
    assert_eq!(
        answered
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
}

#[test]
fn deletion_held_end_survives_store_failure_without_a_second_end() {
    let (mut core, mut effects) = deletion_before_ack(started());
    effects.extend(
        core.handle(Input::StoreFailed { run: R })
            .expect("outcome failed"),
    );
    effects.extend(
        core.handle(Input::RunStored { run: R })
            .expect("failed outcome stored"),
    );
    assert_eq!(core.ending_runs(), 0);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Control(ControlToShell::RunState {
            run: R,
            phase: Phase::Failed,
            ..
        })
    )));
    assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
    effects.extend(
        core.handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete"),
    );
    assert_eq!(effects.last(), Some(&super::run_end(R)));
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(super::Stale));
}

#[test]
fn quieting_and_abandoned_store_ends_stay_behind_deletion_replay() {
    for abandoned in [false, true] {
        let mut core = super::spoken();
        core.handle(Input::Engine {
            key: super::engine_key(&core, R),
            event: TurnEvent::TextDelta("Hello there. ".into()),
        })
        .expect("spoken text");
        let (mut core, mut effects) = deletion_before_ack(core);
        if abandoned {
            effects.extend(
                core.handle(Input::StoreFailed { run: R })
                    .expect("outcome failed"),
            );
            effects.extend(
                core.handle(Input::StoreFailed { run: R })
                    .expect("retry failed"),
            );
        } else {
            effects.extend(
                core.handle(Input::RunStored { run: R })
                    .expect("stored outcome"),
            );
            effects.extend(
                core.handle(Input::SpeechStopped {
                    run: R,
                    cursor: None,
                })
                .expect("quiet"),
            );
        }
        assert_eq!(core.ending_runs(), 0);
        assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
        effects.extend(
            core.handle(Input::Deleted {
                thread: T,
                outcome: kumochi_protocol::message::Deleted::NotHidden,
                last_run: Some(R),
            })
            .expect("refused delete"),
        );
        assert_eq!(effects.last(), Some(&super::run_end(R)));
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            1
        );
    }
}

#[test]
fn a_hidden_thread_never_releases_late_drawing_frames() {
    for outcome in [
        kumochi_protocol::message::Deleted::Hidden,
        kumochi_protocol::message::Deleted::Removed,
    ] {
        let (mut core, effects) = deletion_during_write(outcome);
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
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            1
        );
        assert!(core.drawing_held.is_empty());
        assert_eq!(
            core.handle(Input::Deleted {
                thread: T,
                outcome,
                last_run: Some(R)
            }),
            Err(super::Stale)
        );
    }
}

fn preparing(core: &mut Coordinator, store: &mut Store) -> drawing::PrepareJob {
    core.defer_drawing_preparation();
    let lookup = draw(core);
    let effects = commit(core, store, &lookup);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Data(_)))
    );
    effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PrepareDrawing(job) => Some(job),
            _ => None,
        })
        .expect("deferred preparation")
}

fn preparation_delete(outcome: kumochi_protocol::message::Deleted, notify: Notify) {
    let scratch = Scratch::new("drawing-preparation-delete");
    let mut store = open(&scratch);
    let mut core = started();
    let job = preparing(&mut core, &mut store);
    let mut effects = core
        .handle(Input::StopScenes {
            run: R,
            cuts: SceneCuts::new(Vec::new()).expect("no installed scenes"),
        })
        .expect("stop during preparation");
    assert!(core.drawing_in_flight());
    assert_eq!(core.ending_runs(), 1);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::state(R, T, Phase::Interrupted))
            .count(),
        1
    );
    assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
    if notify == Notify::Nobody {
        effects.extend(core.handle(Input::ShellLost).expect("shell lost"));
    }
    let deleting = core
        .handle(Input::Delete {
            thread: T,
            after_turns: 0,
            resend: false,
        })
        .expect("delete without waiting for preparation");
    assert_eq!(
        deleting
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::Delete { thread: T })))
            .count(),
        1
    );
    assert!(!deleting.iter().any(|effect| *effect == super::run_end(R)));
    effects.extend(deleting);
    effects.extend(
        core.handle(Input::Deleted {
            thread: T,
            outcome,
            last_run: Some(R),
        })
        .expect("successful delete before worker completion"),
    );
    assert_eq!(core.ending_runs(), 0);
    assert!(!core.deletes_pending());
    assert!(core.drawing_in_flight());
    assert_eq!(
        core.handle(Input::Deleted {
            thread: T,
            outcome,
            last_run: Some(R)
        }),
        Err(super::Stale)
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(super::Stale));
    let cancelled = job.run();
    assert!(matches!(
        &cancelled,
        drawing::Input::Prepared {
            outcome: drawing::Prepared::Cancelled,
            ..
        }
    ));
    assert_eq!(core.handle(Input::Drawing(cancelled)), Ok(Vec::new()));
    assert!(core.settled());
    assert!(
        core.drawing_snapshot()
            .expect("no canonical scene revived")
            .scenes
            .is_empty()
    );
    assert!(
        store
            .drawing_scenes()
            .expect("no scene was committed")
            .scenes
            .is_empty()
    );
    assert!(core.drawing_held.is_empty());
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Data(
            DataToShell::SceneBegin { .. }
                | DataToShell::SceneStepCommit { .. }
                | DataToShell::SceneEnd { .. }
                | DataToShell::ScenesRemoved { .. }
        )
    )));
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        usize::from(notify == Notify::Shell),
        "successful deletion must complete the live session's unissued end"
    );
}

#[test]
fn preparation_delete_hidden_completes_the_unissued_end() {
    preparation_delete(kumochi_protocol::message::Deleted::Hidden, Notify::Shell);
}

#[test]
fn preparation_delete_removed_completes_the_unissued_end() {
    preparation_delete(kumochi_protocol::message::Deleted::Removed, Notify::Shell);
}

#[test]
fn preparation_delete_after_loss_sends_no_end() {
    for outcome in [
        kumochi_protocol::message::Deleted::Hidden,
        kumochi_protocol::message::Deleted::Removed,
    ] {
        preparation_delete(outcome, Notify::Nobody);
    }
}

#[test]
fn preparation_delete_refusal_leaves_the_end_with_normal_settlement() {
    let scratch = Scratch::new("drawing-preparation-refused-delete");
    let mut store = open(&scratch);
    let mut core = started();
    let job = preparing(&mut core, &mut store);
    let mut effects = core
        .handle(Input::StopScenes {
            run: R,
            cuts: SceneCuts::new(Vec::new()).expect("no installed scenes"),
        })
        .expect("stop");
    effects.extend(
        core.handle(Input::Delete {
            thread: T,
            after_turns: 0,
            resend: false,
        })
        .expect("delete"),
    );
    effects.extend(
        core.handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete"),
    );
    assert_eq!(core.ending_runs(), 1);
    assert!(core.drawing_in_flight());
    assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
    let settled = core
        .handle(Input::Drawing(job.run()))
        .expect("cancelled preparation");
    assert_eq!(
        settled
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    assert_eq!(
        settled
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { run: R, .. })))
            .count(),
        1
    );
    assert!(!settled.iter().any(|effect| matches!(
        effect,
        Effect::Data(DataToShell::SceneBegin { .. } | DataToShell::SceneStepCommit { .. })
    )));
    core.handle(Input::RunStored { run: R })
        .expect("stored interrupted outcome");
    assert!(core.settled());
    assert!(
        core.drawing_snapshot()
            .expect("no committed scene")
            .scenes
            .is_empty()
    );
}

#[test]
fn deleting_a_preparing_thread_preserves_another_threads_ending() {
    let scratch = Scratch::new("drawing-preparation-other-ending");
    let mut store = open(&scratch);
    let mut core = started();
    let job = preparing(&mut core, &mut store);
    let cuts = SceneCuts::new(Vec::new()).expect("no installed scenes");
    core.handle(Input::StopScenes {
        run: R,
        cuts: cuts.clone(),
    })
    .expect("stop preparing run");
    core.handle(Input::Turn {
        turn: 2,
        thread: None,
        text: "unrelated".into(),
    })
    .expect("other thread");
    let other = kumochi_protocol::message::RunId(R.0 + 1);
    let mut effects = core
        .handle(Input::StopScenes { run: other, cuts })
        .expect("stop queued other run");
    assert_eq!(core.ending_runs(), 2);
    effects.extend(
        core.handle(Input::Delete {
            thread: T,
            after_turns: 0,
            resend: false,
        })
        .expect("delete preparing thread"),
    );
    effects.extend(
        core.handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::Hidden,
            last_run: Some(R),
        })
        .expect("hidden"),
    );
    assert_eq!(core.ending_runs(), 1);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    let stored_turn = core
        .handle(Input::TurnStored { run: other })
        .expect("other turn retained");
    assert_eq!(stored_turn.iter().filter(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { run, .. }) if *run == other)).count(), 1);
    effects.extend(stored_turn);
    effects.extend(
        core.handle(Input::RunStored { run: other })
            .expect("other outcome retained"),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(other))
            .count(),
        1
    );
    assert_eq!(core.handle(Input::Drawing(job.run())), Ok(Vec::new()));
    assert!(core.settled());
    assert!(
        core.drawing_snapshot()
            .expect("no revived scene")
            .scenes
            .is_empty()
    );
}

#[test]
fn deleting_a_done_run_discharges_only_its_still_unissued_end() {
    for (failed, lost) in [(false, false), (true, false), (false, true)] {
        let mut core = started();
        let mut effects = finish(&mut core, Finish::Completed);
        assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
        if failed {
            effects.extend(
                core.handle(Input::StoreFailed { run: R })
                    .expect("Done outcome failed"),
            );
        }
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            usize::from(failed)
        );
        if lost {
            effects.extend(core.handle(Input::ShellLost).expect("lost before deletion"));
        }
        effects.extend(
            core.handle(Input::Delete {
                thread: T,
                after_turns: 0,
                resend: false,
            })
            .expect("delete ending run"),
        );
        let deleted = core
            .handle(Input::Deleted {
                thread: T,
                outcome: kumochi_protocol::message::Deleted::Hidden,
                last_run: Some(R),
            })
            .expect("hidden before release");
        assert_eq!(
            deleted
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            usize::from(!failed && !lost)
        );
        assert!(
            !deleted
                .iter()
                .any(|effect| matches!(effect, Effect::Control(ControlToShell::RunState { .. })))
        );
        effects.extend(deleted);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            usize::from(!lost)
        );
        assert_eq!(core.ending_runs(), 0);
        assert_eq!(core.handle(Input::RunStored { run: R }), Err(super::Stale));
        assert_eq!(
            core.handle(Input::StoreFailed { run: R }),
            Err(super::Stale)
        );
        assert!(
            core.drawing_snapshot()
                .expect("no drawing")
                .scenes
                .is_empty()
        );
    }
}

#[test]
fn deletion_does_not_reissue_an_immediate_interruption_end() {
    let mut core = started();
    let mut effects = core
        .handle(Input::StopScenes {
            run: R,
            cuts: SceneCuts::new(Vec::new()).expect("no scenes"),
        })
        .expect("immediate stop");
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    effects.extend(
        core.handle(Input::Delete {
            thread: T,
            after_turns: 0,
            resend: false,
        })
        .expect("delete"),
    );
    effects.extend(
        core.handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::Removed,
            last_run: Some(R),
        })
        .expect("removed"),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    assert_eq!(core.ending_runs(), 0);
}

#[test]
fn revoking_deletion_held_payload_keeps_its_owed_data_end() {
    for outcome in [
        kumochi_protocol::message::Deleted::NotHidden,
        kumochi_protocol::message::Deleted::Hidden,
    ] {
        let (mut core, mut effects) = deletion_before_ack(started());
        let scene = core.drawing_snapshot().expect("committed scene").scenes[0].scene;
        assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
        core.drawing_clear(vec![scene], &mut effects);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            1
        );
        effects.extend(
            core.handle(Input::ScenesCleared {
                scenes: vec![scene],
                ok: true,
            })
            .expect("cleared payload"),
        );
        effects.extend(
            core.handle(Input::Deleted {
                thread: T,
                outcome,
                last_run: Some(R),
            })
            .expect("delete result"),
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == super::run_end(R))
                .count(),
            1
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Data(
                DataToShell::SceneBegin { .. }
                    | DataToShell::SceneStepCommit { .. }
                    | DataToShell::SceneEnd { .. }
            )
        )));
        assert!(core.drawing_held.is_empty());
        assert!(
            core.drawing_snapshot()
                .expect("cleared scene")
                .scenes
                .is_empty()
        );
    }
}

#[test]
fn revoking_the_last_scene_rehomes_its_end_to_a_surviving_scene() {
    let scratch = Scratch::new("drawing-revoke-survivor");
    let mut store = open(&scratch);
    let mut core = started();
    for (call, scene) in [("first", "one"), ("second", "two")] {
        let batch = draw_named(&mut core, call, scene);
        let effects = settle(&mut core, &mut store, batch);
        for effect in effects {
            if let Effect::Data(
                frame @ (DataToShell::SceneBegin { .. }
                | DataToShell::SceneStepCommit { .. }
                | DataToShell::SceneEnd { .. }),
            ) = effect
            {
                core.drawing_held.push((T, frame, None));
            }
        }
    }
    let snapshot = core.drawing_snapshot().expect("two committed scenes");
    assert_eq!(snapshot.scenes.len(), 2);
    let survivor = snapshot.scenes[0];
    let revoked = snapshot.scenes[1].scene;
    let mut effects = core
        .handle(Input::Delete {
            thread: T,
            after_turns: 0,
            resend: false,
        })
        .expect("accepted delete");
    assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
    core.drawing_clear(vec![revoked], &mut effects);
    assert!(!effects.iter().any(|effect| *effect == super::run_end(R)));
    effects.extend(
        core.handle(Input::ScenesCleared {
            scenes: vec![revoked],
            ok: true,
        })
        .expect("clear stored"),
    );
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete");
    assert_eq!(answered.last(), Some(&super::run_end(R)));
    assert_eq!(
        answered
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    assert_eq!(
        answered.iter().filter(|effect| {
            matches!(effect, Effect::Data(DataToShell::SceneStepCommit { scene, .. }) if *scene == survivor.scene)
        }).count(),
        2
    );
    assert!(!answered.iter().any(|effect| {
        matches!(effect, Effect::Data(DataToShell::SceneBegin { scene, .. } | DataToShell::SceneStepCommit { scene, .. } | DataToShell::SceneEnd { scene, .. }) if *scene == revoked)
    }));
    let recovered = core.drawing_snapshot().expect("survivor recovered");
    assert_eq!(recovered.scenes.len(), 1);
    assert_eq!(recovered.scenes[0], survivor);
    assert!(core.drawing_held.is_empty());
}

#[test]
fn held_frame_capacity_does_not_charge_or_drop_the_owed_end() {
    let (mut core, mut effects) = deletion_before_ack(started());
    let scene = core.drawing_snapshot().expect("committed scene").scenes[0].scene;
    let frame = DataToShell::SceneCutApplied {
        scene,
        revision: 1,
        cut: serde_json::from_str(r#"{"activated":0,"revealed":[]}"#).expect("zero cut"),
    };
    for _ in 3..64 {
        core.drawing_output(
            drawing::Output {
                effects: vec![Effect::Data(frame.clone())],
                calls: Vec::new(),
            },
            &mut effects,
        );
    }
    assert_eq!(core.drawing_held.len(), 64);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Broken(_)))
    );
    core.drawing_output(
        drawing::Output {
            effects: vec![Effect::Data(frame)],
            calls: Vec::new(),
        },
        &mut effects,
    );
    assert_eq!(core.drawing_held.len(), 64);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Broken("too many drawing frames await deletion")
            ))
            .count(),
        1
    );
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete");
    assert_eq!(
        answered
            .iter()
            .filter(|effect| **effect == super::run_end(R))
            .count(),
        1
    );
    assert_eq!(
        answered
            .iter()
            .filter(|effect| matches!(effect, Effect::Data(DataToShell::SceneCutApplied { .. })))
            .count(),
        61
    );
    assert!(core.drawing_held.is_empty());
}

#[test]
fn lost_session_discards_deletion_held_end_with_its_recovery_frames() {
    let (mut core, _) = deletion_before_ack(started());
    let snapshot = core.drawing_snapshot().expect("committed scene");
    let lost = core.handle(Input::ShellLost).expect("lost shell");
    assert!(!lost.iter().any(|effect| *effect == super::run_end(R)));
    core.begin_session();
    core.drawing_described();
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::NotHidden,
            last_run: Some(R),
        })
        .expect("refused delete");
    assert!(
        !answered
            .iter()
            .any(|effect| matches!(effect, Effect::Data(_)))
    );
    assert!(core.drawing_held.is_empty());
    assert_eq!(
        core.drawing_snapshot().expect("recovered scene").scenes,
        snapshot.scenes
    );
}

fn deletion_before_description(outcome: kumochi_protocol::message::Deleted) {
    let (mut core, _) = deletion_before_ack(started());
    let snapshot = core.drawing_snapshot().expect("committed scene");
    assert_eq!(core.ending_runs(), 1);
    let lost = core.handle(Input::ShellLost).expect("lost shell");
    assert!(!lost.iter().any(|effect| *effect == super::run_end(R)));
    core.begin_session();
    let answered = core
        .handle(Input::Deleted {
            thread: T,
            outcome,
            last_run: Some(R),
        })
        .expect("delete before description");
    assert!(
        !answered.iter().any(|effect| *effect == super::run_end(R)),
        "the old session's end has no delivery authority"
    );
    let recovered = core.drawing_snapshot().expect("canonical recovery").scenes;
    if outcome == kumochi_protocol::message::Deleted::NotHidden {
        assert_eq!(recovered, snapshot.scenes);
        let stored = core
            .handle(Input::RunStored { run: R })
            .expect("old outcome stored");
        assert!(!stored.iter().any(|effect| *effect == super::run_end(R)));
    } else {
        assert!(recovered.is_empty());
        assert_eq!(core.handle(Input::RunStored { run: R }), Err(super::Stale));
    }
    core.drawing_described();
    assert!(core.drawing_held.is_empty());
}

#[test]
fn a_refused_delete_before_description_cannot_deliver_the_lost_sessions_end() {
    deletion_before_description(kumochi_protocol::message::Deleted::NotHidden);
}

#[test]
fn a_hidden_delete_before_description_cannot_deliver_the_lost_sessions_end() {
    deletion_before_description(kumochi_protocol::message::Deleted::Hidden);
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
        key: super::engine_key(&core, R),
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
        turn: 1,
        thread: Some(T),
        text: "continue".into(),
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
        br#"{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram","others":"keep"}}"#,
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
        key: super::engine_key(&core, next),
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
        origin: None,
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
fn a_failed_replacement_transaction_preserves_its_cap_victim() {
    let scratch = Scratch::new("drawing-eviction-retry");
    let mut store = open(&scratch);
    let mut core = started();
    let original = four_drawings(&mut core, &mut store);
    assert_eq!(original.len(), 4);
    store.conn_for_tests().execute_batch("CREATE TRIGGER fail_clear BEFORE INSERT ON events WHEN new.kind='scene_cleared' BEGIN SELECT RAISE(FAIL,'private drawing'); END;").expect("fail clear");
    let lookup = draw_named(&mut core, "fifth", "scene-5");
    let first = commit(&mut core, &mut store, &lookup);
    let (serial, write) = pending(&first);
    let Write::Step { clear, .. } = &write else {
        panic!("first-step transaction")
    };
    assert_eq!(clear, &[original[0]]);
    assert!(
        !first
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::ClearScenes { .. })))
    );
    assert!(store.drawing_write(&write).is_err());
    let failed = core
        .handle(Input::DrawingWritten {
            serial,
            reply: Err(()),
        })
        .expect("failed atomic clear");
    assert!(!failed.iter().any(|effect| matches!(
        effect,
        Effect::Data(
            DataToShell::SceneBegin { .. }
                | DataToShell::SceneStepCommit { .. }
                | DataToShell::ScenesRemoved { .. }
                | DataToShell::ScenesReplaced { .. }
        )
    )));
    assert_eq!(core.debt(), 0);
    assert_eq!(recovered_ids(&store), original);
    store
        .conn_for_tests()
        .execute_batch("DROP TRIGGER fail_clear;")
        .expect("allow a fresh replacement call");
    let lookup = draw_named(&mut core, "step-failure", "scene-5");
    let first = commit(&mut core, &mut store, &lookup);
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
    assert_eq!(recovered_ids(&store), original);
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
        {"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"test diagram","others":"keep"}},
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

fn not_returned(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Control(ControlToShell::DrawingNotReturned { thread: T, run: R })
            )
        })
        .count()
}

fn call(core: &mut Coordinator, args: &[u8]) -> Vec<Effect> {
    core.handle(Input::Drawing(drawing::Input::Call {
        thread: T,
        run: R,
        call: CallId::new("draw").expect("call"),
        args: args.to_vec(),
    }))
    .expect("fresh call")
}

fn finish(core: &mut Coordinator, finish: Finish) -> Vec<Effect> {
    core.handle(Input::Engine {
        key: super::engine_key(core, R),
        event: TurnEvent::Finished(finish),
    })
    .expect("round finished")
}

#[test]
fn a_finished_run_whose_draw_calls_committed_no_step_says_so_once() {
    for args in [
        &br#"{"records":[]}"#[..],
        b"not an envelope",
        br#"{"records":[{"step":{"id":"s1","put":[]}}]}"#,
    ] {
        let mut core = started();
        let mut effects = call(&mut core, args);
        effects.extend(finish(&mut core, Finish::Completed));
        assert_eq!(
            not_returned(&effects),
            0,
            "the notice waits for the stored outcome"
        );
        effects.extend(stored(&mut core));
        assert_eq!(not_returned(&effects), 1);
    }
}

#[test]
fn a_done_run_whose_outcome_write_fails_adds_no_drawing_notice() {
    let mut core = started();
    let mut effects = call(&mut core, br#"{"records":[]}"#);
    effects.extend(finish(&mut core, Finish::Completed));
    effects.extend(
        core.handle(Input::StoreFailed { run: R })
            .expect("the outcome write failed"),
    );
    effects.extend(stored(&mut core));
    assert_eq!(not_returned(&effects), 0);
}

/// Answers every drawing write `batch` starts, one after another, keeping every effect.
fn settle(core: &mut Coordinator, store: &mut Store, mut batch: Vec<Effect>) -> Vec<Effect> {
    let mut effects = Vec::new();
    while batch
        .iter()
        .any(|effect| matches!(effect, Effect::Store(StoreOp::Drawing { .. })))
    {
        effects.extend(batch.iter().cloned());
        batch = commit(core, store, &batch);
    }
    effects.extend(batch);
    effects
}

fn stored(core: &mut Coordinator) -> Vec<Effect> {
    core.handle(Input::RunStored { run: R })
        .expect("stored outcome")
}

fn ended_done(effects: &[Effect]) -> bool {
    effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Store(StoreOp::EndRun {
                end: RunEnd::Done,
                ..
            })
        )
    })
}

#[test]
fn a_committed_step_a_stop_or_a_failed_run_adds_no_drawing_notice() {
    let scratch = Scratch::new("drawing-returned");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let first = commit(&mut core, &mut store, &lookup);
    let mut effects = finish(&mut core, Finish::Completed);
    let second = commit(&mut core, &mut store, &first);
    let last = commit(&mut core, &mut store, &second);
    assert!(ended_done(&last));
    effects.extend(first.into_iter().chain(second).chain(last));
    effects.extend(stored(&mut core));
    assert_eq!(not_returned(&effects), 0);

    let scratch = Scratch::new("drawing-cleared");
    let mut store = open(&scratch);
    let mut core = started();
    let lookup = draw(&mut core);
    let mut batch = commit(&mut core, &mut store, &lookup);
    batch.extend(core.handle(Input::ClearVisible).expect("user clear"));
    batch.extend(finish(&mut core, Finish::Completed));
    let mut effects = settle(&mut core, &mut store, batch);
    assert!(ended_done(&effects));
    effects.extend(stored(&mut core));
    assert_eq!(not_returned(&effects), 0);

    let mut core = started();
    let mut effects = call(&mut core, br#"{"records":[]}"#);
    effects.extend(
        core.handle(Input::Stop {
            run: R,
            revealed: vec![],
        })
        .expect("stop"),
    );
    assert_eq!(not_returned(&effects), 0);

    let mut core = started();
    let mut effects = call(&mut core, br#"{"records":[]}"#);
    effects.extend(finish(
        &mut core,
        Finish::Failed(crate::engines::EngineFailure::Provider),
    ));
    assert_eq!(not_returned(&effects), 0);
}

#[test]
fn committed_steps_the_shell_refused_count_as_no_drawing_unless_one_was_shown() {
    for shown in [false, true] {
        let scratch = Scratch::new(if shown {
            "drawing-kept"
        } else {
            "drawing-refused"
        });
        let mut store = open(&scratch);
        let mut core = started();
        let lookup = draw(&mut core);
        let mut effects = settle(&mut core, &mut store, lookup);
        let scene = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Data(DataToShell::SceneStepCommit { scene, .. }) => Some(*scene),
                _ => None,
            })
            .expect("a committed step");
        if shown {
            effects.extend(
                core.handle(Input::Drawing(drawing::Input::StepShown {
                    run: R,
                    scene,
                    index: 0,
                }))
                .expect("a committed step"),
            );
        }
        let failed = core
            .handle(Input::Drawing(drawing::Input::StepFailed {
                run: R,
                scene,
                index: u32::from(shown),
                reason: kumochi_protocol::message::SceneFailure::PaintWork,
                cut: kumochi_scene::StopCut {
                    activated: u32::from(shown),
                    revealed: kumochi_scene::text::Bounded::new(Vec::new()).expect("none"),
                },
            }))
            .expect("a committed step");
        effects.extend(settle(&mut core, &mut store, failed));
        let finished = finish(&mut core, Finish::Completed);
        effects.extend(settle(&mut core, &mut store, finished));
        assert!(ended_done(&effects));
        effects.extend(stored(&mut core));
        assert_eq!(not_returned(&effects), usize::from(!shown));
    }
}

/// A replacement whose first step the shell couldn't show took the thread's earlier drawing
/// away. The user hears of it when the run fails or finishes, never after ending it themselves.
#[test]
fn a_lost_replacement_is_told_only_when_the_user_did_not_end_its_run() {
    for (end, told) in [
        ("stop", false),
        ("replaced", false),
        ("failed", true),
        ("done", true),
    ] {
        let scratch = Scratch::new(&format!("drawing-loss-{end}"));
        let mut store = open(&scratch);
        let mut core = started();
        let kept = draw_named(&mut core, "kept", "kept");
        let mut effects = settle(&mut core, &mut store, kept);
        let args = serde_json::to_vec(&json!({"records":[
            {"begin":{"v":1,"scene":"new","surface":"overlay","alt":"new"}},
            {"step":{"id":"n1","put":[{"shape":{"id":"n","kind":"rect","box":[0,0,10,10]}}]}},
            {"end":{}}
        ]}))
        .expect("arguments");
        let replacing = call(&mut core, &args);
        effects.extend(settle(&mut core, &mut store, replacing));
        let scene = effects
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::Data(DataToShell::SceneStepCommit { scene, .. }) => Some(*scene),
                _ => None,
            })
            .expect("the replacement's first step");
        let failed = core
            .handle(Input::Drawing(drawing::Input::StepFailed {
                run: R,
                scene,
                index: 0,
                reason: kumochi_protocol::message::SceneFailure::Timeout,
                cut: kumochi_scene::StopCut {
                    activated: 0,
                    revealed: kumochi_scene::text::Bounded::new(Vec::new()).expect("none"),
                },
            }))
            .expect("a committed step");
        let failed = settle(&mut core, &mut store, failed);
        assert_eq!(
            replacement_lost(&failed),
            0,
            "the run could still repair it"
        );
        let ended = match end {
            "stop" => core
                .handle(Input::Stop {
                    run: R,
                    revealed: vec![],
                })
                .expect("stop"),
            "replaced" => core
                .handle(Input::Turn {
                    turn: 2,
                    thread: Some(T),
                    text: "next".into(),
                })
                .expect("a turn is never stale"),
            "failed" => finish(
                &mut core,
                Finish::Failed(crate::engines::EngineFailure::Provider),
            ),
            _ => finish(&mut core, Finish::Completed),
        };
        let ended = settle(&mut core, &mut store, ended);
        assert_eq!(replacement_lost(&ended), usize::from(told), "{end}");
    }
}

fn replacement_lost(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Control(ControlToShell::DrawingReplacementFailed { thread: T, run: R })
            )
        })
        .count()
}

/// A step answer from a run before this session is stale, whatever its scene and index.
#[test]
fn a_ready_step_from_before_this_session_is_stale() {
    use kumochi_protocol::message::{RunId, SceneId};
    let mut core = super::fresh();
    let ready = Input::Drawing(drawing::Input::StepReady {
        run: RunId(9),
        scene: SceneId(1),
        index: 0,
    });
    assert_eq!(core.handle(ready), Err(crate::coordinator::Stale));
}
