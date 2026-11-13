use std::cell::RefCell;

use super::*;
use crate::coordinator::{Availability, StoreOp};
use crate::runtime::data::DataQueue;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use kumochi_protocol::message::GestureId;

const KEY: HoldKey = HoldKey {
    epoch: 0,
    gesture: GestureId(9),
};

#[test]
fn failed_hold_status_logs_only_closed_reason_without_identifiers() {
    let captured = test_support::Captured::default();
    let _logging = captured.install();
    let (out, mut output) = mpsc::channel(8);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    for reason in [
        HoldFailure::NotLoaded,
        HoldFailure::Busy,
        HoldFailure::Unavailable,
        HoldFailure::InvalidAudio,
        HoldFailure::TooLong,
        HoldFailure::TimedOut,
        HoldFailure::ChoiceChanged,
        HoldFailure::DecodeFailed,
    ] {
        Brain::hold_status(KEY, HoldStatus::Failed(reason), &mut link).unwrap();
        assert_eq!(status(&mut output), HoldStatus::Failed(reason));
    }
    let text = captured.text();
    let reasons = [
        "NotLoaded",
        "Busy",
        "Unavailable",
        "InvalidAudio",
        "TooLong",
        "TimedOut",
        "ChoiceChanged",
        "DecodeFailed",
    ];
    let diagnostics = text.lines().collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), reasons.len());
    for (line, reason) in diagnostics.iter().zip(reasons) {
        let message = line.split("held speech input failed ").nth(1).unwrap();
        assert_eq!(message, format!("stage=Status reason={reason}"));
    }
}

#[tokio::test]
async fn stale_recognition_and_failed_preparation_log_fixed_refusals() {
    let scratch = Scratch::new("hold-refusal-diagnostics");
    let mut brain = test_support::brain(&scratch, "");
    let captured = test_support::Captured::default();
    let _logging = captured.install();
    let (out, mut output) = mpsc::channel(8);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    brain.holds.high = KEY.gesture.0;
    brain
        .hold_control(
            &ControlToBrain::VoiceHoldRecognized {
                gesture: KEY.gesture,
                thread: None,
                selection: 1,
            },
            &mut link,
        )
        .unwrap();
    assert!(output.is_empty());
    brain.hold_prepared(KEY, Err(()), &mut link).unwrap();
    assert!(output.is_empty());
    recording(&mut brain);
    brain.hold_prepared(KEY, Err(()), &mut link).unwrap();
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::ChoiceChanged)
    );
    let text = captured.text();
    let diagnostics = text
        .lines()
        .filter_map(|line| {
            line.split("held speech input refused ")
                .nth(1)
                .or_else(|| line.split("held speech input failed ").nth(1))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        [
            "stage=Admission reason=StaleGesture",
            "stage=Preparation reason=StaleCompletion",
            "stage=Preparation reason=ReadOrCancellation",
            "stage=Status reason=ChoiceChanged",
        ]
    );
    brain.close().await;
}

#[tokio::test]
async fn hold_to_talk_off_refuses_before_admission() {
    let scratch = Scratch::new("hold-disabled");
    let (mut brain, _release, mut decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    std::fs::write(
        scratch.dir().join(kumochi_protocol::constants::CONFIG_FILE),
        "[voice]\nhold_to_talk = false\n",
    )
    .unwrap();
    brain.config = scratch.config();
    let (out, mut output) = mpsc::channel(8);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert!(
        brain
            .hold_control(
                &ControlToBrain::VoiceHoldRecognized {
                    gesture: KEY.gesture,
                    thread: None,
                    selection: 1,
                },
                &mut link
            )
            .unwrap()
    );
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::NotLoaded)
    );
    assert!(brain.holds.pending.is_none());
    assert_eq!(brain.holds.debt, None);
    assert_eq!(brain.coordinator.current_run(), None);
    assert_eq!(decoded.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    let pending = brain.config.stage(SettingsOp::SetHoldToTalk(true)).unwrap();
    pending.write().unwrap();
    brain.config.commit(pending);
    assert!(brain.hold_to_talk_enabled());
    brain
        .hold_control(
            &ControlToBrain::VoiceHoldRecognized {
                gesture: KEY.gesture,
                thread: None,
                selection: 1,
            },
            &mut link,
        )
        .unwrap();
    assert!(brain.holds.pending.is_none());
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain.close().await;
}

#[tokio::test]
async fn queued_and_in_flight_hold_to_talk_off_block_before_the_save_commits() {
    use crate::runtime::settings::{Change, Gate, Queued};
    for [first, second] in [[false, true], [true, false]] {
        let scratch = Scratch::new("hold-disable-save");
        let (mut brain, _release, _decoded, _fetch) =
            crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
        assert!(brain.hold_to_talk_enabled());
        let release = brain.hold_config_write();
        brain.settings_ops.push_back(Queued {
            change: Change::User(SettingsOp::SetHoldToTalk(first)),
            epoch: 0,
        });
        assert_eq!(brain.hold_to_talk_enabled(), first);
        brain.pump_settings(None).unwrap();
        assert!(brain.config.view().hold_to_talk);
        brain.settings_ops.push_back(Queued {
            change: Change::User(SettingsOp::SetHoldToTalk(second)),
            epoch: 0,
        });
        let finish = brain.hold_config_write();
        let mut release = Some(release);
        let (out, mut output) = mpsc::channel(8);
        let queue = RefCell::new(DataQueue::new(0));
        let ready = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        for phase in 0..2 {
            assert!(!brain.hold_to_talk_enabled());
            let gesture = GestureId(phase + 1);
            brain
                .hold_control(
                    &ControlToBrain::VoiceHoldRecognized {
                        gesture,
                        thread: None,
                        selection: 1,
                    },
                    &mut link,
                )
                .unwrap();
            let bytes = output.try_recv().unwrap();
            assert_eq!(
                kumochi_protocol::frame::decode::<ControlToShell>(
                    &bytes[kumochi_protocol::frame::HEADER_LEN..]
                )
                .unwrap(),
                ControlToShell::VoiceHoldStatus {
                    gesture,
                    status: HoldStatus::Failed(HoldFailure::NotLoaded)
                }
            );
            assert!(brain.holds.pending.is_none());
            assert_eq!(brain.coordinator.current_run(), None);
            if phase == 0 {
                release.take().unwrap().send(Gate::Write).unwrap();
                let done = super::super::settings::written(&mut brain.config_write).await;
                brain.settings_done(done, None).unwrap();
            }
        }
        finish.send(Gate::Write).unwrap();
        brain.drain_settings().await;
        assert_eq!(brain.config.view().hold_to_talk, second);
        assert_eq!(brain.hold_to_talk_enabled(), second);
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        brain.close().await;
    }
}

#[derive(Clone, Copy)]
enum AdmissionStage {
    Grant,
    Transfer,
}

#[expect(
    clippy::too_many_lines,
    reason = "the same four blocked states exercise grant and transfer"
)]
async fn admission_boundaries(stage: AdmissionStage) {
    use crate::coordinator::Coordinator;
    use crate::runtime::data::Owner;
    use crate::runtime::{DATA_CAP, DEBT_WATERMARK, MAX_ENDING, Recovery, Seen};
    use kumochi_protocol::message::{RunId, SceneId, ThreadId};
    for blocked in 0..4 {
        let scratch = Scratch::new("hold-admission-capacity");
        let (mut brain, _release, mut decoded, _fetch) =
            crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
        let mut observed = brain.observe();
        let (out, mut output) = mpsc::channel(16);
        let queue = RefCell::new(DataQueue::new(0));
        let ready = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        link.recovery = Some(Recovery::new(&[], 1024));
        recording(&mut brain);
        match blocked {
            0 => {
                queue.borrow_mut().push(
                    Owner::Marker,
                    vec![0; DATA_CAP - size_of::<(Owner, Vec<u8>)>()],
                );
                assert_eq!(queue.borrow().bytes(), DATA_CAP);
            }
            1 => {
                for turn in 1..=u32::try_from(MAX_ENDING + 1).unwrap() {
                    brain
                        .coordinator
                        .handle(Input::Turn {
                            turn,
                            thread: None,
                            text: "held writes".into(),
                        })
                        .unwrap();
                }
                assert_eq!(brain.coordinator.ending_runs(), MAX_ENDING);
            }
            2 => {
                brain.coordinator.failed_drawings(
                    (1..=u64::try_from(DEBT_WATERMARK).unwrap())
                        .map(SceneId)
                        .collect(),
                );
                assert_eq!(brain.coordinator.debt(), DEBT_WATERMARK);
            }
            _ => link.recovery = None,
        }
        let previous = brain.coordinator.current_run();
        match stage {
            AdmissionStage::Grant => {
                let hold = brain.holds.pending.as_mut().unwrap();
                hold.ask = None;
                hold.audio = None;
                brain.holds.debt = None;
                brain.holds.gate.active.store(0, Ordering::SeqCst);
                brain.hold_voice_changed(&mut link).unwrap();
            }
            AdmissionStage::Transfer => {
                let (reply, receive) = oneshot::channel();
                let hold = brain.holds.pending.as_mut().unwrap();
                hold.audio = None;
                hold.decode = Some(receive);
                brain.holds.stopped(KEY);
                reply
                    .send(SttResult::Text("recognized words".into()))
                    .unwrap();
                let completed = next(&mut brain.holds).await;
                brain.hold_event(completed, &mut link).unwrap();
            }
        }
        assert_eq!(status(&mut output), HoldStatus::Failed(HoldFailure::Busy));
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert!(brain.holds.pending.is_none());
        assert_eq!(brain.holds.debt, None);
        assert_eq!(brain.coordinator.current_run(), previous);
        assert!(brain.round.is_none());
        assert_eq!(decoded.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        while let Ok(event) = observed.try_recv() {
            assert!(!matches!(
                event,
                Seen::StoreOpQueued(StoreOp::BeginTurn { .. })
            ));
        }
        brain.coordinator =
            Coordinator::ready_for_tests(ThreadId(1), ThreadId(1), RunId(0), Vec::new());
        brain.close().await;
        let store = crate::store::Store::open(&scratch.db()).unwrap();
        assert_eq!(
            store
                .conn_for_tests()
                .query_row(
                    "SELECT count(*) FROM events WHERE kind = 'user'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn ordinary_admission_limits_refuse_before_microphone_grant() {
    Box::pin(admission_boundaries(AdmissionStage::Grant)).await;
}

#[tokio::test]
async fn prepared_hold_across_loading_is_admitted_before_first_serve_iteration() {
    use crate::runtime::{Recovery, voice};
    use kumochi_protocol::message::PartState;
    let scratch = Scratch::new("hold-loaded-ready");
    let (mut brain, release, _decoded, _fetch) =
        crate::runtime::tests::holds::loading_input(&scratch, Vec::new(), "unused");
    let resume = crate::voice::stt::hold_before_serving(brain.hold_voice().1.unwrap());
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = Some(Recovery::new(&[], 1024));
    recording(&mut brain);
    let hold = brain.holds.pending.as_mut().unwrap();
    hold.ask = None;
    hold.audio = None;
    brain.holds.debt = None;
    brain.holds.gate.active.store(0, Ordering::SeqCst);
    brain.hold_voice_changed(&mut link).unwrap();
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    release.send(()).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), voice::next(&mut brain.voice))
        .await
        .unwrap();
    brain.voice_event(event, Some(&mut link)).unwrap();
    let grant = status(&mut output);
    let bytes = output.try_recv().unwrap();
    let view = kumochi_protocol::frame::decode::<ControlToShell>(
        &bytes[kumochi_protocol::frame::HEADER_LEN..],
    )
    .unwrap();
    let ask = brain.holds.pending.as_ref().and_then(|hold| hold.ask);
    brain.hold_voice_changed(&mut link).unwrap();
    let more = output.try_recv();
    resume();
    brain.holds.lost();
    brain.close().await;
    assert_eq!(grant, HoldStatus::Admitted);
    assert!(matches!(view, ControlToShell::Voice(view) if view.speech_input == PartState::Ready));
    assert_eq!(ask, Some(1));
    assert_eq!(more, Err(mpsc::error::TryRecvError::Empty));
}

#[tokio::test]
async fn stop_before_admission_cancels_the_pending_hold_and_owes_no_debt() {
    use crate::runtime::{Recovery, voice};
    use kumochi_protocol::message::PartState;
    let scratch = Scratch::new("hold-stop-before-admission");
    let (mut brain, release, _decoded, _fetch) =
        crate::runtime::tests::holds::loading_input(&scratch, Vec::new(), "unused");
    let resume = crate::voice::stt::hold_before_serving(brain.hold_voice().1.unwrap());
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = Some(Recovery::new(&[], 1024));
    recording(&mut brain);
    let hold = brain.holds.pending.as_mut().unwrap();
    hold.ask = None;
    hold.audio = None;
    brain.holds.debt = None;
    brain.holds.gate.active.store(0, Ordering::SeqCst);
    brain.hold_voice_changed(&mut link).unwrap();
    assert!(
        brain
            .hold_control(
                &ControlToBrain::VoiceHoldStopped {
                    gesture: KEY.gesture
                },
                &mut link
            )
            .unwrap()
    );
    accepted(&mut output, KEY.gesture);
    assert_eq!(status(&mut output), HoldStatus::Cancelled);
    assert!(brain.holds.pending.is_none());
    release.send(()).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), voice::next(&mut brain.voice))
        .await
        .unwrap();
    brain.voice_event(event, Some(&mut link)).unwrap();
    let bytes = output.try_recv().unwrap();
    let view = kumochi_protocol::frame::decode::<ControlToShell>(
        &bytes[kumochi_protocol::frame::HEADER_LEN..],
    )
    .unwrap();
    assert!(matches!(view, ControlToShell::Voice(view) if view.speech_input == PartState::Ready));
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(brain.holds.debt, None);
    resume();
    brain.close().await;
}

#[tokio::test]
async fn admission_ties_device_debt_to_the_admitting_shell_process() {
    use crate::runtime::{Recovery, voice};
    use std::sync::atomic::AtomicU8;
    let scratch = Scratch::new("hold-debt-peer");
    let (mut brain, release, _decoded, _fetch) =
        crate::runtime::tests::holds::loading_input(&scratch, Vec::new(), "unused");
    let resume = crate::voice::stt::hold_before_serving(brain.hold_voice().1.unwrap());
    let exited = Arc::new(AtomicU8::new(0));
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&exited)));
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = Some(Recovery::new(&[], 1024));
    recording(&mut brain);
    let hold = brain.holds.pending.as_mut().unwrap();
    hold.ask = None;
    hold.audio = None;
    brain.holds.debt = None;
    brain.holds.gate.active.store(0, Ordering::SeqCst);
    brain.hold_voice_changed(&mut link).unwrap();
    release.send(()).unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), voice::next(&mut brain.voice))
        .await
        .unwrap();
    brain.voice_event(event, Some(&mut link)).unwrap();
    assert_eq!(status(&mut output), HoldStatus::Admitted);
    resume();
    brain.holds.lost();
    brain.connected_shell(ShellProcess::fixture(20, Arc::new(AtomicU8::new(0))));
    assert_eq!(brain.holds.debt, Some(KEY));
    exited.store(1, Ordering::Release);
    brain.connected_shell(ShellProcess::fixture(20, Arc::new(AtomicU8::new(0))));
    assert_eq!(brain.holds.debt, None);
    brain.close().await;
}

#[tokio::test]
async fn ordinary_admission_limits_refuse_decoded_transfer_without_a_turn() {
    Box::pin(admission_boundaries(AdmissionStage::Transfer)).await;
}

async fn expired_finalization(last: ControlToBrain) {
    use crate::runtime::Recovery;
    let scratch = Scratch::new("hold-expired-finalization");
    let (mut brain, _release, mut decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, vec![21, -37], "not submitted").await;
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = Some(Recovery::new(&[], 1024));
    recording(&mut brain);
    brain
        .hold_event(packet(KEY.gesture, 0, true, vec![21, -37]), &mut link)
        .unwrap();
    let stop_last = matches!(&last, ControlToBrain::VoiceHoldStopped { .. });
    let first = if stop_last {
        ControlToBrain::KeyUp {
            gesture: KEY.gesture,
            t_qpc: 1,
            chunks: 1,
        }
    } else {
        ControlToBrain::VoiceHoldStopped {
            gesture: KEY.gesture,
        }
    };
    brain.hold_control(&first, &mut link).unwrap();
    if !stop_last {
        accepted(&mut output, KEY.gesture);
    }
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    let owed = stop_last.then_some(KEY);
    assert_eq!(brain.holds.debt, owed);
    brain
        .hold_control(
            &ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(8),
            },
            &mut link,
        )
        .unwrap();
    accepted(&mut output, GestureId(8));
    assert_eq!(brain.holds.debt, owed);
    tokio::time::pause();
    brain.holds.pending.as_mut().unwrap().deadline = Instant::now() + Duration::from_secs(1);
    let (messages, mut controls) = mpsc::channel(1);
    messages.try_send(last.clone()).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::select! {
        biased;
        Some(message) = controls.recv() => { brain.hold_control(&message, &mut link).unwrap(); }
        _ = next(&mut brain.holds) => panic!("the core's ready control precedes its ready hold timer"),
    }
    if stop_last {
        accepted(&mut output, KEY.gesture);
    }
    let terminal = status(&mut output);
    assert_eq!(terminal, HoldStatus::Failed(HoldFailure::TimedOut));
    assert!(brain.holds.pending.is_none());
    assert_eq!(brain.holds.debt, None);
    assert_eq!(brain.coordinator.current_run(), None);
    assert!(brain.round.is_none());
    assert_eq!(decoded.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain.hold_control(&first, &mut link).unwrap();
    brain.hold_control(&last, &mut link).unwrap();
    accepted(&mut output, KEY.gesture);
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    tokio::time::resume();
    brain.close().await;
    let store = crate::store::Store::open(&scratch.db()).unwrap();
    assert_eq!(
        store
            .conn_for_tests()
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'user'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn expired_hold_stop_ready_with_deadline_does_not_start_decode() {
    Box::pin(expired_finalization(ControlToBrain::VoiceHoldStopped {
        gesture: KEY.gesture,
    }))
    .await;
}

#[tokio::test]
async fn expired_hold_key_up_after_stop_does_not_extend_the_deadline() {
    Box::pin(expired_finalization(ControlToBrain::KeyUp {
        gesture: KEY.gesture,
        t_qpc: 1,
        chunks: 1,
    }))
    .await;
}

fn recording(brain: &mut Brain) {
    let cancel = Arc::new(AtomicBool::new(false));
    let StoreOp::ReadHold(read) = brain
        .coordinator
        .prepare_hold(
            KEY,
            None,
            3,
            brain.snapshot(Origin::Kumo),
            Arc::clone(&cancel),
        )
        .unwrap()
    else {
        panic!("pinned read")
    };
    brain.holds.pending = Some(Hold {
        key: KEY,
        thread: None,
        cancel,
        snapshot: Some(read.snapshot),
        audio: Some(audio::Audio::new()),
        ask: Some(1),
        released: false,
        deadline: Instant::now() + Duration::from_secs(70),
        decode: None,
        terminal: None,
        screen: ScreenPart::None,
    });
    brain.holds.debt = Some(KEY);
    brain
        .holds
        .gate
        .active
        .store(KEY.gesture.0, Ordering::SeqCst);
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture supplies the protocol's last bit"
)]
fn packet(gesture: GestureId, seq: u32, last: bool, samples: Vec<i16>) -> Event {
    Event::Packet(Packet(MicPcm {
        gesture,
        rate: 16_000,
        seq,
        last,
        samples,
    }))
}

fn status(output: &mut mpsc::Receiver<Vec<u8>>) -> HoldStatus {
    let bytes = output.try_recv().unwrap();
    match kumochi_protocol::frame::decode(&bytes[kumochi_protocol::frame::HEADER_LEN..]).unwrap() {
        ControlToShell::VoiceHoldStatus { gesture, status } => {
            assert_eq!(gesture, KEY.gesture);
            status
        }
        other => panic!("hold status, got {other:?}"),
    }
}

fn accepted(output: &mut mpsc::Receiver<Vec<u8>>, gesture: GestureId) {
    let bytes = output.try_recv().unwrap();
    assert_eq!(
        kumochi_protocol::frame::decode::<ControlToShell>(
            &bytes[kumochi_protocol::frame::HEADER_LEN..]
        )
        .unwrap(),
        ControlToShell::VoiceHoldStopAccepted { gesture }
    );
}

#[tokio::test]
async fn duplicate_stop_acceptance_never_poisons_or_restarts_the_pending_audio() {
    let scratch = Scratch::new("hold-duplicate-stop");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    recording(&mut brain);
    for _ in 0..2 {
        assert!(
            brain
                .hold_control(
                    &ControlToBrain::VoiceHoldStopped {
                        gesture: KEY.gesture
                    },
                    &mut link
                )
                .unwrap()
        );
        accepted(&mut output, KEY.gesture);
        assert_eq!(brain.holds.debt, None);
        assert!(brain.holds.pending.is_some());
        assert_eq!(brain.coordinator.current_run(), None);
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }
    let audio = brain
        .holds
        .pending
        .as_mut()
        .unwrap()
        .audio
        .as_mut()
        .unwrap();
    assert_eq!(audio.end(0), Ok(()));
    assert_eq!(audio.take(), Some(Vec::new()));
    brain.close().await;
}

#[tokio::test]
async fn malformed_audio_is_terminal_and_old_gestures_cannot_poison_the_owner() {
    let scratch = Scratch::new("hold-terminal-framing");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    for fault in 0..6 {
        recording(&mut brain);
        brain
            .hold_event(packet(GestureId(8), 99, true, vec![11]), &mut link)
            .unwrap();
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        brain
            .hold_event(packet(KEY.gesture, 0, false, vec![13; 320]), &mut link)
            .unwrap();
        let bad = match fault {
            0 => packet(KEY.gesture, 0, true, vec![7]),
            1 => packet(KEY.gesture, 2, true, vec![7]),
            2 => packet(KEY.gesture, 1, false, vec![7]),
            3 => Event::Packet(Packet(MicPcm {
                gesture: KEY.gesture,
                rate: 8_000,
                seq: 1,
                last: true,
                samples: vec![7],
            })),
            4 => packet(KEY.gesture, 1, true, Vec::new()),
            _ => packet(KEY.gesture, MAX_HOLD_CHUNKS, true, vec![7]),
        };
        brain.hold_event(bad, &mut link).unwrap();
        assert_eq!(
            status(&mut output),
            HoldStatus::Failed(HoldFailure::InvalidAudio)
        );
        brain
            .hold_event(packet(KEY.gesture, 1, true, vec![7]), &mut link)
            .unwrap();
        brain
            .hold_control(
                &ControlToBrain::KeyUp {
                    gesture: KEY.gesture,
                    t_qpc: 1,
                    chunks: 2,
                },
                &mut link,
            )
            .unwrap();
        assert!(brain.holds.pending.is_none());
        assert_eq!(brain.holds.debt, Some(KEY));
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        brain
            .hold_control(
                &ControlToBrain::VoiceHoldStopped {
                    gesture: KEY.gesture,
                },
                &mut link,
            )
            .unwrap();
        assert_eq!(brain.holds.debt, None);
        accepted(&mut output, KEY.gesture);
    }
    assert_eq!(brain.coordinator.current_run(), None);
    brain.close().await;
}

#[tokio::test]
async fn bounded_pcm_overflow_is_scoped_and_never_waits_for_turn_capacity() {
    let scratch = Scratch::new("hold-overflow");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    recording(&mut brain);
    let ingress = brain.holds.ingress();
    for seq in 0..9 {
        ingress.push(MicPcm {
            gesture: KEY.gesture,
            rate: 16_000,
            seq,
            last: false,
            samples: vec![19; 320],
        });
    }
    assert_eq!(brain.holds.receive.len(), 8);
    let event = next(&mut brain.holds).await;
    assert!(matches!(event, Event::Overflow(9)));
    brain.hold_event(event, &mut link).unwrap();
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::InvalidAudio)
    );
    brain.holds.lost();
    assert_eq!(brain.holds.receive.len(), 0);
    recording(&mut brain);
    brain.holds.pending.as_mut().unwrap().key.gesture = GestureId(10);
    brain.hold_event(Event::Overflow(9), &mut link).unwrap();
    assert!(brain.holds.pending.is_some());
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn deadline_and_eof_never_fabricate_a_physical_stop_receipt() {
    let scratch = Scratch::new("hold-device-debt");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    recording(&mut brain);
    brain
        .hold_control(
            &ControlToBrain::KeyUp {
                gesture: KEY.gesture,
                t_qpc: 1,
                chunks: 1,
            },
            &mut link,
        )
        .unwrap();
    brain
        .hold_control(
            &ControlToBrain::VoiceHoldStopped {
                gesture: KEY.gesture,
            },
            &mut link,
        )
        .unwrap();
    let deadline = brain.holds.pending.as_ref().unwrap().deadline;
    accepted(&mut output, KEY.gesture);
    assert!(deadline <= Instant::now() + Duration::from_millis(u64::from(HOLD_DRAIN_TIMEOUT_MS)));
    brain.holds.pending.as_mut().unwrap().deadline = Instant::now();
    assert!(matches!(next(&mut brain.holds).await, Event::Deadline));
    brain.hold_event(Event::Deadline, &mut link).unwrap();
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::TimedOut)
    );
    assert_eq!(brain.holds.debt, None);
    recording(&mut brain);
    let cancelled = Arc::clone(&brain.holds.pending.as_ref().unwrap().cancel);
    brain.holds.lost();
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(brain.holds.pending.is_none());
    assert_eq!(brain.holds.debt, Some(KEY));
    brain.epoch = 1;
    brain.holds.opened();
    brain.holds.stopped(HoldKey { epoch: 1, ..KEY });
    assert_eq!(brain.holds.debt, Some(KEY));
    brain.holds.stopped(KEY);
    assert_eq!(brain.holds.debt, None);
    brain.close().await;
}

#[tokio::test]
async fn outer_debug_errors_and_tap_omit_text_and_byte_sentinels() {
    use kumochi_protocol::frame::Pipe;
    use kumochi_protocol::message::DataToBrain;
    use kumochi_protocol::tap::Direction;
    let scratch = Scratch::new("hold-redaction");
    let mut brain = test_support::brain(&scratch, "");
    recording(&mut brain);
    let marker = "held-content-sentinel";
    let samples = marker.bytes().map(i16::from).collect::<Vec<_>>();
    let message = DataToBrain::MicPcm(MicPcm {
        gesture: KEY.gesture,
        rate: 16_000,
        seq: 0,
        last: true,
        samples: samples.clone(),
    });
    let Event::Packet(packet) = packet(KEY.gesture, 0, true, samples.clone()) else {
        unreachable!()
    };
    brain
        .holds
        .pending
        .as_mut()
        .unwrap()
        .audio
        .as_mut()
        .unwrap()
        .push(&packet.0)
        .unwrap();
    let path = scratch.db().with_file_name("hold.tap");
    let mut tap = Tap::to_file(std::fs::File::create(&path).unwrap());
    tap.record(Pipe::Data, Direction::ToBrain, &message, 8);
    let text = format!(
        "{brain:?} {packet:?} {:?} {:?} {:?}",
        SttResult::Text(marker.into()),
        HoldFailure::InvalidAudio,
        crate::voice::stt::SttBusy
    );
    assert!(!text.contains(marker));
    assert!(!text.contains(&format!("{samples:?}")));
    let bytes = std::fs::read(path).unwrap();
    assert!(
        !bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    );
    let pcm = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
    assert!(!bytes.windows(pcm.len()).any(|window| window == pcm));
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn preparing_release_or_cancel_is_final_and_refusals_consume_the_gesture() {
    let scratch = Scratch::new("hold-preparing");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    for cancel in [false, true] {
        recording(&mut brain);
        brain.holds.debt = None;
        let hold = brain.holds.pending.as_mut().unwrap();
        let snapshot = hold.snapshot.take().unwrap();
        hold.audio = None;
        hold.ask = None;
        let message = if cancel {
            ControlToBrain::GestureCancel {
                gesture: KEY.gesture,
            }
        } else {
            ControlToBrain::KeyUp {
                gesture: KEY.gesture,
                t_qpc: 1,
                chunks: 0,
            }
        };
        brain.hold_control(&message, &mut link).unwrap();
        assert_eq!(status(&mut output), HoldStatus::Cancelled);
        brain.hold_prepared(KEY, Ok(snapshot), &mut link).unwrap();
        assert!(brain.holds.pending.is_none());
        assert_eq!(brain.holds.debt, None);
        assert_eq!(brain.coordinator.current_run(), None);
    }
    let message = ControlToBrain::VoiceHoldRecognized {
        gesture: GestureId(KEY.gesture.0 + 1),
        thread: None,
        selection: 9,
    };
    brain.hold_control(&message, &mut link).unwrap();
    let bytes = output.try_recv().unwrap();
    assert_eq!(
        kumochi_protocol::frame::decode::<ControlToShell>(
            &bytes[kumochi_protocol::frame::HEADER_LEN..]
        )
        .unwrap(),
        ControlToShell::VoiceHoldStatus {
            gesture: GestureId(KEY.gesture.0 + 1),
            status: HoldStatus::Failed(HoldFailure::NotLoaded),
        }
    );
    brain.hold_control(&message, &mut link).unwrap();
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(brain.holds.pending.is_none());
    brain.close().await;
}

#[tokio::test]
async fn final_packet_and_key_up_without_source_receipt_cannot_schedule_decode() {
    let scratch = Scratch::new("hold-needs-stop");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    recording(&mut brain);
    brain
        .hold_event(packet(KEY.gesture, 0, true, vec![21, -37]), &mut link)
        .unwrap();
    brain
        .hold_control(
            &ControlToBrain::KeyUp {
                gesture: KEY.gesture,
                t_qpc: 1,
                chunks: 1,
            },
            &mut link,
        )
        .unwrap();
    let hold = brain.holds.pending.as_ref().unwrap();
    assert!(hold.decode.is_none());
    assert!(hold.audio.is_some());
    assert_eq!(brain.holds.debt, Some(KEY));
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain
        .hold_control(
            &ControlToBrain::VoiceHoldStopped {
                gesture: KEY.gesture,
            },
            &mut link,
        )
        .unwrap();
    accepted(&mut output, KEY.gesture);
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::NotLoaded)
    );
    assert_eq!(brain.holds.debt, None);
    brain.close().await;
}

#[tokio::test]
async fn saturated_audio_still_reads_credit_behind_a_full_turn_queue() {
    use kumochi_protocol::frame::{self, Pipe};
    use kumochi_protocol::message::{DataToBrain, MAX_PENDING_TURNS, RunId};
    use tokio::io::AsyncWriteExt as _;
    let scratch = Scratch::new("hold-credit");
    let mut brain = test_support::brain(&scratch, "");
    recording(&mut brain);
    let ingress = brain.holds.ingress();
    let (turns, mut held_turns) = mpsc::channel(MAX_PENDING_TURNS);
    for turn in 0..u32::try_from(MAX_PENDING_TURNS).unwrap() {
        turns
            .try_send(Input::Turn {
                turn,
                thread: None,
                text: "held".into(),
            })
            .unwrap();
    }
    let (pages, _held_pages) = mpsc::channel(1);
    let queue = RefCell::new(DataQueue::new(10));
    queue
        .borrow_mut()
        .push(super::super::Owner::Run(RunId(1)), vec![1; 10]);
    assert_eq!(
        queue.borrow_mut().pop(std::time::Instant::now()),
        Some(vec![1; 10])
    );
    queue.borrow_mut().wrote();
    queue
        .borrow_mut()
        .push(super::super::Owner::Run(RunId(1)), vec![2; 5]);
    assert_eq!(queue.borrow_mut().pop(std::time::Instant::now()), None);
    let ready = Notify::new();
    let (mut reader, mut writer) = tokio::io::duplex(32);
    let read = super::super::read_data(
        &mut reader,
        &turns,
        &pages,
        &queue,
        &ready,
        0,
        Some(&ingress),
    );
    let write = async {
        for seq in 0..9 {
            let message = DataToBrain::MicPcm(MicPcm {
                gesture: KEY.gesture,
                rate: 16_000,
                seq,
                last: false,
                samples: vec![19; 320],
            });
            writer
                .write_all(&frame::encode(&message, Pipe::Data).unwrap())
                .await
                .unwrap();
        }
        writer
            .write_all(&frame::encode(&DataToBrain::Credit { bytes: 5 }, Pipe::Data).unwrap())
            .await
            .unwrap();
        drop(writer);
    };
    let (ended, ()) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(read, write) })
            .await
            .unwrap();
    assert_eq!(ended, super::super::Ended::Lost);
    assert_eq!(brain.holds.receive.len(), 8);
    assert_eq!(brain.holds.gate.overflow.load(Ordering::SeqCst), 9);
    assert_eq!(held_turns.len(), MAX_PENDING_TURNS);
    assert_eq!(
        queue.borrow_mut().pop(std::time::Instant::now()),
        Some(vec![2; 5])
    );
    assert!(matches!(
        held_turns.try_recv(),
        Ok(Input::Turn { turn: 0, .. })
    ));
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn accepted_connection_change_away_and_back_and_late_result_cannot_revive_a_hold() {
    use crate::context::ConnectionTag;
    use crate::coordinator::Ready;
    let scratch = Scratch::new("hold-choice-revision");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    recording(&mut brain);
    let flag = Arc::clone(&brain.holds.pending.as_ref().unwrap().cancel);
    let mut away = Ready::fixture();
    away.connection = ConnectionTag(21);
    brain.offline(Input::Engines(Availability::Ready(away)));
    brain.offline(Input::Engines(Availability::Ready(Ready::fixture())));
    assert!(flag.load(Ordering::SeqCst));
    brain
        .hold_event(
            Event::Decoded(KEY, SttResult::Text("obsolete".into())),
            &mut link,
        )
        .unwrap();
    assert_eq!(
        status(&mut output),
        HoldStatus::Failed(HoldFailure::ChoiceChanged)
    );
    recording(&mut brain);
    let newer = HoldKey {
        gesture: GestureId(10),
        ..KEY
    };
    brain.holds.pending.as_mut().unwrap().key = newer;
    brain
        .hold_event(
            Event::Decoded(KEY, SttResult::Text("obsolete".into())),
            &mut link,
        )
        .unwrap();
    assert_eq!(brain.holds.pending.as_ref().unwrap().key, newer);
    assert_eq!(brain.coordinator.current_run(), None);
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn cancel_model_default_and_unload_changes_are_terminal_before_their_handlers() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
    let scratch = Scratch::new("hold-control-fences");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let choice = ModelChoice {
        model: ModelSlug::new("fixture-model").unwrap(),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    let thread = brain.config.view().main_thread;
    let messages = [
        (
            ControlToBrain::GestureCancel {
                gesture: KEY.gesture,
            },
            HoldStatus::Cancelled,
        ),
        (
            ControlToBrain::Settings(SettingsOp::SetDefaultModel(choice.clone())),
            HoldStatus::Failed(HoldFailure::ChoiceChanged),
        ),
        (
            ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false)),
            HoldStatus::Failed(HoldFailure::NotLoaded),
        ),
        (
            ControlToBrain::ThreadOp(ThreadOp::SetModel { thread, choice }),
            HoldStatus::Failed(HoldFailure::ChoiceChanged),
        ),
        (
            ControlToBrain::Settings(SettingsOp::RemoveVoice(
                kumochi_protocol::voice::VoicePart::SpeechInput,
            )),
            HoldStatus::Failed(HoldFailure::NotLoaded),
        ),
    ];
    for (message, expected) in messages {
        recording(&mut brain);
        brain.holds.pending.as_mut().unwrap().thread = Some(thread);
        let flag = Arc::clone(&brain.holds.pending.as_ref().unwrap().cancel);
        brain.hold_control(&message, &mut link).unwrap();
        if let Some(input) = super::super::controls::control_input(message).unwrap() {
            brain.hold_fence(&input);
            brain.hold_voice_changed(&mut link).unwrap();
        }
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(status(&mut output), expected);
        brain
            .hold_event(
                Event::Decoded(KEY, SttResult::Text("obsolete".into())),
                &mut link,
            )
            .unwrap();
        assert!(brain.holds.pending.is_none());
        assert_eq!(brain.coordinator.current_run(), None);
    }
    brain.close().await;
}

/// Leaves a recording hold on `thread` whose screen part is `screen`.
fn screen_hold(brain: &mut Brain, key: HoldKey, screen: ScreenPart) {
    recording(brain);
    let thread = brain.config.view().main_thread;
    let hold = brain.holds.pending.as_mut().unwrap();
    hold.key = key;
    hold.thread = Some(thread);
    hold.screen = screen;
}

fn waiting() -> ScreenPart {
    ScreenPart::Waiting {
        generation: Some(crate::screen::Generation(0)),
    }
}

enum Closer {
    Control(ControlToBrain),
    ShellLost,
    Event(Event),
    Lost,
}

#[tokio::test]
async fn every_closer_of_a_waiting_screen_part_retires_its_frame_once() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
    let scratch = Scratch::new("hold-screen-closers");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let choice = ModelChoice {
        model: ModelSlug::new("fixture-model").unwrap(),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    let thread = brain.config.view().main_thread;
    let closers = [
        Closer::Control(ControlToBrain::GestureCancel {
            gesture: KEY.gesture,
        }),
        Closer::Control(ControlToBrain::Settings(SettingsOp::SetDefaultModel(
            choice.clone(),
        ))),
        Closer::Control(ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false))),
        Closer::Control(ControlToBrain::Settings(SettingsOp::RemoveVoice(
            kumochi_protocol::voice::VoicePart::SpeechInput,
        ))),
        Closer::Control(ControlToBrain::ThreadOp(ThreadOp::SetModel {
            thread,
            choice,
        })),
        Closer::ShellLost,
        Closer::Event(Event::Deadline),
        Closer::Event(Event::Decoded(KEY, SttResult::Text("words".into()))),
        Closer::Lost,
    ];
    for closer in closers {
        screen_hold(&mut brain, KEY, waiting());
        let submits = matches!(closer, Closer::Event(Event::Decoded(..)));
        match closer {
            Closer::Control(message) => {
                brain.hold_control(&message, &mut link).unwrap();
                if let Some(input) = super::super::controls::control_input(message).unwrap() {
                    brain.hold_fence(&input);
                }
            }
            Closer::ShellLost => brain.hold_fence(&Input::ShellLost),
            Closer::Event(event) => brain.hold_event(event, &mut link).unwrap(),
            Closer::Lost => brain.holds.lost(),
        }
        assert!(
            brain.holds.pending.as_ref().is_none_or(|hold| matches!(
                hold.screen,
                ScreenPart::Closed(screen::Class::NotReady)
            ))
        );
        crate::screen::release_next(&mut rig, KEY).await;
        brain.holds.lost();
        while let Ok(bytes) = output.try_recv() {
            let busy = ControlToShell::VoiceHoldStatus {
                gesture: KEY.gesture,
                status: HoldStatus::Failed(HoldFailure::Busy),
            };
            let frame = kumochi_protocol::frame::decode::<ControlToShell>(
                &bytes[kumochi_protocol::frame::HEADER_LEN..],
            );
            assert!(
                !submits || frame.ok() != Some(busy),
                "the transcript reached submit"
            );
        }
    }
    let voice = HoldKey {
        epoch: 0,
        gesture: GestureId(1),
    };
    screen_hold(&mut brain, voice, ScreenPart::None);
    brain.holds.lost();
    screen_hold(
        &mut brain,
        voice,
        ScreenPart::Ready(crate::screen::ScreenContext::fixture(b"jpeg")),
    );
    brain.holds.lost();
    let last = HoldKey {
        epoch: 0,
        gesture: GestureId(2),
    };
    screen_hold(&mut brain, last, waiting());
    brain.holds.lost();
    crate::screen::release_next(&mut rig, last).await;
    brain.close().await;
}

#[test]
fn a_screen_hold_prints_no_image_bytes() {
    let scratch = Scratch::new("hold-screen-debug");
    let mut brain = test_support::brain(&scratch, "");
    let marker = b"KUMOCHI-HOLD-SCREEN-MARKER";
    let ready = || ScreenPart::Ready(crate::screen::ScreenContext::fixture(marker));
    screen_hold(&mut brain, KEY, ready());
    let texts = [
        format!("{:?}", brain.holds),
        format!("{:?}", brain.holds.pending),
        format!("{:?}", ready()),
    ];
    for text in texts {
        assert!(!text.contains("KUMOCHI-HOLD-SCREEN-MARKER"), "{text}");
        assert!(!text.contains(&format!("{marker:?}")[1..20]), "{text}");
    }
    brain.holds.pending = None;
}
