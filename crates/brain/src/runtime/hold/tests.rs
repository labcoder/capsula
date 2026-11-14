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
        *brain.coordinator =
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
        lease: LEASE,
        stop: Arc::default(),
    }
}

const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];
const LEASE: kumochi_protocol::message::LeaseGeneration =
    kumochi_protocol::message::LeaseGeneration(4);

fn ready() -> ScreenPart {
    ScreenPart::Ready(crate::screen::ScreenContext::fixture(JPEG))
}

enum Closer {
    Control(ControlToBrain),
    Fence(Box<Input>),
    Event(Event),
    /// The serve loop's queued turn and the settings queue's model change both invalidate.
    Invalidate(HoldStatus),
    Lost,
}

#[tokio::test]
async fn every_closer_of_a_waiting_screen_part_retires_its_frame_once() {
    let scratch = Scratch::new("hold-screen-closers");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let closers = closers(brain.config.view().main_thread);
    for (index, closer) in closers.into_iter().enumerate() {
        screen_hold(&mut brain, KEY, waiting());
        let submits = matches!(closer, Closer::Event(Event::Decoded(..)));
        match closer {
            Closer::Control(message) => {
                brain.hold_control(&message, &mut link).unwrap();
                if let Some(input) = super::super::controls::control_input(message).unwrap() {
                    brain.hold_fence(&input);
                }
            }
            Closer::Fence(input) => brain.hold_fence(&input),
            Closer::Event(event) => brain.hold_event(event, &mut link).unwrap(),
            Closer::Invalidate(status) => brain.holds.invalidate(status),
            Closer::Lost => brain.holds.lost(),
        }
        assert!(
            brain.holds.pending.as_ref().is_none_or(|hold| matches!(
                hold.screen,
                ScreenPart::Closed(crate::context::ScreenClass::NotReady)
            )),
            "closer {index}"
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
    screen_hold(&mut brain, voice, ready());
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

#[tokio::test]
async fn a_ready_image_goes_to_its_run_and_a_waiting_part_closes_before_its_run_takes_it() {
    use crate::context::{Jpeg, Screen, ScreenClass};
    let scratch = Scratch::new("hold-screen-submit");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    screen_hold(&mut brain, KEY, ready());
    let image = Jpeg::new(JPEG.to_vec()).unwrap();
    assert!(matches!(brain.holds.submit_screen(), Some(Screen::Image(sent)) if sent == image));
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::None
    ));
    brain.holds.lost();
    screen_hold(&mut brain, KEY, waiting());
    assert!(matches!(
        brain.holds.submit_screen(),
        Some(Screen::NotIncluded(ScreenClass::NotReady))
    ));
    crate::screen::release_next(&mut rig, KEY).await;
    brain.holds.lost();
    for part in [ScreenPart::None, ScreenPart::Discarded] {
        screen_hold(&mut brain, KEY, part);
        assert!(brain.holds.submit_screen().is_none());
        brain.holds.lost();
    }
    screen_hold(&mut brain, KEY, ready());
    brain.holds.invalidate(HoldStatus::Cancelled);
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Discarded
    ));
    assert!(brain.holds.submit_screen().is_none());
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn the_transcript_hands_a_ready_image_to_its_run() {
    let scratch = Scratch::new("hold-screen-run");
    let mut brain = test_support::brain(&scratch, "");
    let (out, _output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    screen_hold(&mut brain, KEY, ready());
    brain
        .hold_event(
            Event::Decoded(KEY, SttResult::Text("words".into())),
            &mut link,
        )
        .unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(crate::context::Screen::Image(image))
            if *image == crate::context::Jpeg::new(JPEG.to_vec()).unwrap()
    ));
    brain.close().await;
}

#[test]
#[should_panic(expected = "a hold ended past both of its screen part's closers")]
fn a_hold_dropped_with_a_live_screen_part_fails_in_tests() {
    drop(Hold {
        key: KEY,
        thread: None,
        cancel: Arc::default(),
        snapshot: None,
        audio: None,
        ask: None,
        released: false,
        deadline: Instant::now(),
        decode: None,
        terminal: None,
        screen: waiting(),
    });
}

#[test]
fn a_screen_hold_prints_no_image_bytes() {
    let scratch = Scratch::new("hold-screen-debug");
    let mut brain = test_support::brain(&scratch, "");
    let marker = b"KUMOCHI-HOLD-SCREEN-MARKER";
    let part = || ScreenPart::Ready(crate::screen::ScreenContext::fixture(marker));
    screen_hold(&mut brain, KEY, part());
    let screen = crate::context::Screen::Image(
        crate::context::Jpeg::new([&[0xFF, 0xD8, 0xFF][..], marker].concat()).unwrap(),
    );
    let texts = [
        format!("{:?}", brain.holds),
        format!("{:?}", brain.holds.pending),
        format!("{:?}", part()),
        format!("{screen:?}"),
    ];
    for text in texts {
        assert!(!text.contains("KUMOCHI-HOLD-SCREEN-MARKER"), "{text}");
        assert!(!text.contains(&format!("{marker:?}")[1..20]), "{text}");
    }
    brain.holds.lost();
}

/// Every way a hold ends, through the entry point production uses for it.
fn closers(thread: ThreadId) -> Vec<Closer> {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
    let choice = ModelChoice {
        model: ModelSlug::new("fixture-model").unwrap(),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    let fence = |input| Closer::Fence(Box::new(input));
    vec![
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
        fence(Input::ShellLost),
        fence(Input::Turn {
            turn: 1,
            thread: None,
            text: "typed".into(),
        }),
        fence(Input::Ask {
            turn: 1,
            scene: kumochi_protocol::message::SceneId(1),
            element: kumochi_scene::Handle::new(1, 0).unwrap(),
        }),
        fence(Input::Delete {
            thread,
            after_turns: 0,
            resend: false,
        }),
        fence(Input::Engines(crate::coordinator::Availability::Ready(
            crate::coordinator::Ready {
                connection: crate::context::ConnectionTag(77),
                ..crate::coordinator::Ready::fixture()
            },
        ))),
        Closer::Event(Event::Deadline),
        Closer::Event(Event::Decoded(KEY, SttResult::Text("words".into()))),
        Closer::Invalidate(HoldStatus::Cancelled),
        Closer::Invalidate(HoldStatus::Failed(HoldFailure::ChoiceChanged)),
        Closer::Lost,
    ]
}

#[test]
fn every_worker_answer_closes_a_part_with_its_class() {
    use crate::context::ScreenClass::{
        CantShare, Changed, NotReady, NothingThere, TooLarge, Unreadable,
    };
    use crate::screen::{Closed, Refusal, WorkerLost};
    let refused = Closed::Refused;
    let cases = [
        (Closed::Busy, NotReady),
        (Closed::Fault(5), NotReady),
        (Closed::Lost(WorkerLost::Lost), NotReady),
        (refused(Refusal::Expired), NotReady),
        (refused(Refusal::Unsupported), NotReady),
        (refused(Refusal::NoWindow), NothingThere),
        (refused(Refusal::Minimized), CantShare),
        (refused(Refusal::Cloaked), CantShare),
        (refused(Refusal::Elevated), CantShare),
        (refused(Refusal::Protected), CantShare),
        (refused(Refusal::Excluded), CantShare),
        (refused(Refusal::Unmasked), Unreadable),
        (refused(Refusal::Ambiguous), Unreadable),
        (refused(Refusal::TooLarge), TooLarge),
        (refused(Refusal::Invalid), TooLarge),
        (refused(Refusal::WindowChanged), Changed),
        (refused(Refusal::DisplayChanged), Changed),
        (refused(Refusal::Stale), Changed),
    ];
    for (closed, class) in cases {
        assert_eq!(screen::class(closed), class, "{closed:?}");
    }
}

#[tokio::test]
async fn only_a_waiting_parts_own_worker_finishes_it() {
    use crate::context::ScreenClass;
    use crate::screen::Generation;
    let scratch = Scratch::new("hold-screen-events");
    let mut brain = test_support::brain(&scratch, "");
    let event = |generation, outcome| ScreenEvent {
        key: KEY,
        generation: Some(Generation(generation)),
        outcome,
    };
    let sealed = || Outcome::Sealed(crate::screen::ScreenContext::fixture(JPEG));
    screen_hold(
        &mut brain,
        KEY,
        ScreenPart::Waiting {
            generation: Some(Generation(1)),
            lease: LEASE,
            stop: Arc::default(),
        },
    );
    brain.holds.screen_event(event(0, sealed()));
    brain
        .holds
        .screen_event(event(0, Outcome::Closed(ScreenClass::Changed)));
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Waiting {
            generation: Some(Generation(1)),
            ..
        }
    ));
    brain.holds.screen_event(event(1, sealed()));
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Ready(_)
    ));
    brain.holds.lost();
    screen_hold(&mut brain, KEY, ScreenPart::Closed(ScreenClass::NotReady));
    brain.holds.screen_event(event(0, sealed()));
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    brain.holds.lost();
    brain.holds.screen_event(event(0, sealed()));
    assert!(brain.holds.pending.is_none());
    brain.close().await;
}

#[tokio::test]
async fn a_frame_held_after_its_part_closed_is_retired_on_the_worker_that_holds_it() {
    let scratch = Scratch::new("hold-screen-late-held");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    screen_hold(
        &mut brain,
        KEY,
        ScreenPart::Closed(crate::context::ScreenClass::NotReady),
    );
    brain.holds.screen_event(ScreenEvent {
        key: KEY,
        generation: Some(crate::screen::Generation(1)),
        outcome: Outcome::Held,
    });
    // Only a retire naming worker 1 ends worker 0 when worker 0 answers it.
    crate::screen::release_next(&mut rig, KEY).await;
    tokio::time::timeout(Duration::from_secs(5), rig.held())
        .await
        .expect("the retire named the frame's worker");
    rig.confirm.send(true).expect("fixture listens");
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn a_part_closed_during_its_capture_retires_the_frame_without_a_walk() {
    let scratch = Scratch::new("hold-screen-stop");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    screen_hold(&mut brain, KEY, ScreenPart::None);
    let point = kumochi_protocol::geometry::PhysicalPoint([5, 5]);
    assert!(brain.holds.start_screen(KEY, point, LEASE));
    assert!(
        !brain.holds.start_screen(KEY, point, LEASE),
        "one part per hold"
    );
    let (id, capture) = rig.next().await;
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    crate::screen::hold_frame(&rig, id, capture).await;
    let Event::Screen(held) = super::next(&mut brain.holds).await else {
        panic!("not the task's answer");
    };
    assert!(matches!(held.outcome, Outcome::Held));
    brain.holds.screen_event(held);
    for _ in 0..2 {
        crate::screen::release_next(&mut rig, KEY).await;
    }
    brain.close().await;
}

#[tokio::test]
async fn a_lapse_of_the_parts_own_lease_before_its_capture_closes_it() {
    use crate::context::ScreenClass;
    use kumochi_protocol::message::LeaseGeneration;
    let scratch = Scratch::new("hold-screen-lapse");
    let mut brain = test_support::brain(&scratch, "");
    let (out, _output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let lapse = |lease| ControlToBrain::ExclusionLapsed { lease };
    let part = |generation| ScreenPart::Waiting {
        generation,
        lease: LEASE,
        stop: Arc::default(),
    };
    for (generation, lapsed, closes) in [
        (None, LEASE, true),
        (None, LeaseGeneration(5), false),
        (Some(crate::screen::Generation(0)), LEASE, false),
    ] {
        screen_hold(&mut brain, KEY, part(generation));
        let handled = brain.hold_control(&lapse(lapsed), &mut link).unwrap();
        let screen = &brain.holds.pending.as_ref().unwrap().screen;
        assert_eq!(handled, closes, "{generation:?} {lapsed:?}");
        assert_eq!(
            matches!(screen, ScreenPart::Closed(ScreenClass::NotReady)),
            closes,
            "{generation:?} {lapsed:?}"
        );
        brain.holds.lost();
    }
    brain.close().await;
}

#[tokio::test]
async fn a_screen_hold_shares_the_voice_admission_and_releases_its_lease_when_refused() {
    let scratch = Scratch::new("hold-screen-admission");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let message = ControlToBrain::HoldRecognized {
        gesture: KEY.gesture,
        thread: None,
        selection: 1,
        pointer: kumochi_protocol::geometry::PhysicalPoint([5, 5]),
        lease: LEASE,
        t_qpc: 1,
    };
    let failed = ControlToShell::CaptureFailed {
        capture_for: kumochi_protocol::message::CaptureFor::Gesture(KEY.gesture),
        reason: kumochi_protocol::message::CaptureFailure::Unavailable,
    };
    assert!(brain.hold_control(&message, &mut link).unwrap());
    assert_eq!(
        control(&mut output),
        ControlToShell::VoiceHoldStatus {
            gesture: KEY.gesture,
            status: HoldStatus::Failed(HoldFailure::NotLoaded),
        }
    );
    assert_eq!(control(&mut output), failed);
    assert!(brain.hold_control(&message, &mut link).unwrap());
    assert_eq!(control(&mut output), failed, "a stale gesture's lease too");
    assert!(output.is_empty());
    assert!(brain.holds.pending.is_none());
    brain.close().await;
}

/// Sol takes images and is the default; Luna doesn't. A brain with a worker client and that catalog.
fn sees_on_sol(
    brain: &mut Brain,
) -> (
    crate::supervise::fixture::Rig<crate::screen::Holds>,
    ModelChoice,
    ModelChoice,
) {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelInfo, ModelName, ModelSlug};
    let rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let choice = |slug: &str| ModelChoice {
        model: ModelSlug::new(slug).unwrap(),
        effort: Effort::Low,
        connection: ConnectionKind::ChatGpt,
    };
    let (sol, luna) = (choice("gpt-6-sol"), choice("gpt-6-luna"));
    let info = |choice: &ModelChoice, name: &str| ModelInfo {
        slug: choice.model.clone(),
        display_name: ModelName::new(name).unwrap(),
    };
    brain.account.read_catalog(
        vec![info(&sol, "GPT-6 Sol"), info(&luna, "GPT-6 Luna")],
        vec![sol.model.clone()],
    );
    set(brain, SettingsOp::SetDefaultModel(sol.clone()));
    (rig, sol, luna)
}

fn set(brain: &mut Brain, op: SettingsOp) {
    let pending = brain.config.stage(op).unwrap();
    pending.write().unwrap();
    brain.config.commit(pending);
}

#[tokio::test]
async fn the_threads_choice_rechecks_the_gate_and_a_closed_gate_closes_the_part_with_its_reason() {
    use crate::context::ScreenClass;
    let scratch = Scratch::new("hold-screen-pinned");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (out, _output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    let (mut rig, sol, luna) = sees_on_sol(&mut brain);
    // The pin, the switch, and how the part ends: open (`None`), or closed with its line, if any.
    let cases = [
        (Some(luna), true, Some(Some(ScreenClass::ModelCantSee))),
        (Some(sol.clone()), true, None),
        (None, true, Some(Some(ScreenClass::NotReady))),
        (Some(sol), false, Some(None)),
    ];
    for (index, (pin, on, ends)) in cases.into_iter().enumerate() {
        set(&mut brain, SettingsOp::SetScreenHolds(on));
        for part in [ready(), waiting()] {
            let waits = matches!(part, ScreenPart::Waiting { .. });
            let before = format!("{part:?}");
            recording(&mut brain);
            brain.holds.debt = None;
            let hold = brain.holds.pending.as_mut().unwrap();
            let mut snapshot = hold.snapshot.take().unwrap();
            snapshot.defaults.choice.clone_from(&pin);
            hold.audio = None;
            hold.ask = None;
            hold.screen = part;
            brain.hold_prepared(KEY, Ok(snapshot), &mut link).unwrap();
            let screen = &brain.holds.pending.as_ref().unwrap().screen;
            match ends {
                None => assert_eq!(format!("{screen:?}"), before, "case {index}"),
                Some(Some(class)) => assert!(
                    matches!(screen, ScreenPart::Closed(closed) if *closed == class),
                    "case {index}: {screen:?}"
                ),
                Some(None) => assert!(
                    matches!(screen, ScreenPart::Discarded),
                    "case {index}: {screen:?}"
                ),
            }
            brain.holds.lost();
            if waits {
                crate::screen::release_next(&mut rig, KEY).await;
            }
        }
    }
    brain.close().await;
}

#[tokio::test]
async fn a_screen_worker_that_cant_start_leaves_screen_holds_unsupported() {
    use kumochi_protocol::frame;
    use kumochi_protocol::message::{ControlToShell, ScreenHolds, ScreenOff};
    let scratch = Scratch::new("screen-worker-missing");
    let mut brain = test_support::brain(&scratch, "");
    brain.holds.window_capture = true;
    let jobs = Arc::new(crate::platform::Job::new().expect("job"));
    crate::start_screen(&mut brain, scratch.dir().join("missing.exe"), &jobs);
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    brain.screen_holds_changed(&mut link).expect("room");
    let bytes = output.try_recv().expect("the gate");
    assert_eq!(
        frame::decode::<ControlToShell>(&bytes[frame::HEADER_LEN..]).expect("decodes"),
        ControlToShell::ScreenHolds(ScreenHolds::Off(ScreenOff::Unsupported))
    );
    brain.close().await;
}

/// Every control message the shell was sent since the last call.
fn controls(output: &mut mpsc::Receiver<Vec<u8>>) -> Vec<ControlToShell> {
    use kumochi_protocol::frame;
    std::iter::from_fn(|| output.try_recv().ok())
        .map(|bytes| frame::decode(&bytes[frame::HEADER_LEN..]).expect("decodes"))
        .collect()
}

/// Every gate message the shell was sent since the last call.
fn gates(output: &mut mpsc::Receiver<Vec<u8>>) -> Vec<ScreenHolds> {
    controls(output)
        .into_iter()
        .filter_map(|message| match message {
            ControlToShell::ScreenHolds(state) => Some(state),
            _ => None,
        })
        .collect()
}

fn unanswered() -> ScreenPart {
    ScreenPart::Waiting {
        generation: None,
        lease: LEASE,
        stop: Arc::default(),
    }
}

#[tokio::test]
async fn a_system_that_cant_capture_ends_the_hold_not_ready_and_holds_stay_unsupported() {
    use crate::screen::{Closed, Refusal};
    use kumochi_protocol::message::ScreenOff;
    let scratch = Scratch::new("hold-screen-cant-capture");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let (rig, sol, _luna) = sees_on_sol(&mut brain);
    brain.holds.window_capture = true;
    brain.screen_holds_changed(&mut link).unwrap();
    assert!(matches!(gates(&mut output)[..], [ScreenHolds::On { .. }]));
    screen_hold(&mut brain, KEY, unanswered());
    let unsupported = || ScreenEvent {
        key: KEY,
        generation: None,
        outcome: screen::ending(Closed::Refused(Refusal::Unsupported)),
    };
    brain.hold_screen_event(unsupported(), &mut link).unwrap();
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    assert_eq!(
        controls(&mut output),
        [
            ControlToShell::CaptureFailed {
                capture_for: CaptureFor::Gesture(KEY.gesture),
                reason: CaptureFailure::Unavailable,
            },
            ControlToShell::ScreenHolds(ScreenHolds::Off(ScreenOff::Unsupported)),
        ]
    );
    brain.hold_screen_event(unsupported(), &mut link).unwrap();
    assert_eq!(gates(&mut output), []);
    for on in [false, true] {
        let pending = brain.config.stage(SettingsOp::SetScreenHolds(on)).unwrap();
        pending.write().unwrap();
        brain
            .settings_done((0, None, None, Ok((pending, Ok(())))), Some(&mut link))
            .unwrap();
        brain.screen_holds_changed(&mut link).unwrap();
    }
    brain.attach_screen(Arc::new(HoldClient::new(rig.handle)));
    brain.screen_holds_changed(&mut link).unwrap();
    assert_eq!(gates(&mut output), []);
    assert_eq!(
        brain.screen_gate(Some(&sol)),
        ScreenHolds::Off(ScreenOff::Unsupported)
    );
}

#[tokio::test]
async fn a_capture_the_system_refuses_comes_back_from_the_task_as_unsupported() {
    use crate::screen::Refusal;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new("hold-screen-refused");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    screen_hold(&mut brain, KEY, ScreenPart::None);
    let point = kumochi_protocol::geometry::PhysicalPoint([5, 5]);
    assert!(brain.holds.start_screen(KEY, point, LEASE));
    let (id, _capture) = rig.next().await;
    rig.reply(id, &Reply::Refused(Refusal::Unsupported)).await;
    let Event::Screen(answer) = super::next(&mut brain.holds).await else {
        panic!("not the task's answer");
    };
    assert!(matches!(answer.outcome, Outcome::Unsupported));
    assert_eq!(answer.generation, None);
    brain.holds.screen_event(answer);
    brain.close().await;
}

#[tokio::test]
async fn a_capture_fault_ends_the_hold_not_ready_and_leaves_screen_holds_on() {
    use crate::screen::Closed;
    let scratch = Scratch::new("hold-screen-fault");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let (_rig, sol, _luna) = sees_on_sol(&mut brain);
    brain.holds.window_capture = true;
    brain.screen_holds_changed(&mut link).unwrap();
    gates(&mut output);
    screen_hold(&mut brain, KEY, unanswered());
    let event = ScreenEvent {
        key: KEY,
        generation: None,
        outcome: screen::ending(Closed::Fault(0x8000_4001_u32.cast_signed())),
    };
    brain.hold_screen_event(event, &mut link).unwrap();
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    assert_eq!(gates(&mut output), []);
    assert!(matches!(
        brain.screen_gate(Some(&sol)),
        ScreenHolds::On { .. }
    ));
}

#[tokio::test]
async fn without_window_capture_the_gate_reads_unsupported_whatever_the_client() {
    use kumochi_protocol::message::{ScreenHolds, ScreenOff};
    let scratch = Scratch::new("hold-screen-unsupported");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (_rig, sol, _luna) = sees_on_sol(&mut brain);
    brain.holds.window_capture = true;
    assert!(matches!(
        brain.screen_gate(Some(&sol)),
        ScreenHolds::On { .. }
    ));
    brain.holds.window_capture = false;
    assert!(matches!(
        brain.screen_gate(Some(&sol)),
        ScreenHolds::Off(ScreenOff::Unsupported)
    ));
    brain.close().await;
}

#[tokio::test]
async fn the_gate_is_sent_once_per_connection_and_again_only_when_it_changes() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let scratch = Scratch::new("hold-screen-sent");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let (_rig, _sol, luna) = sees_on_sol(&mut brain);
    let off = |why| ControlToShell::ScreenHolds(ScreenHolds::Off(why));
    brain.screen_holds_changed(&mut link).unwrap();
    assert_eq!(
        control(&mut output),
        ControlToShell::ScreenHolds(ScreenHolds::On {
            model: ModelName::new("GPT-6 Sol").unwrap()
        })
    );
    brain.screen_holds_changed(&mut link).unwrap();
    assert!(output.is_empty(), "an unchanged gate isn't resent");
    set(&mut brain, SettingsOp::SetScreenHolds(false));
    brain.screen_holds_changed(&mut link).unwrap();
    assert_eq!(control(&mut output), off(ScreenOff::TurnedOff));
    set(&mut brain, SettingsOp::SetScreenHolds(true));
    set(&mut brain, SettingsOp::SetDefaultModel(luna));
    brain.screen_holds_changed(&mut link).unwrap();
    assert_eq!(control(&mut output), off(ScreenOff::ModelCantSee));
    brain.holds.opened();
    brain.screen_holds_changed(&mut link).unwrap();
    assert_eq!(
        control(&mut output),
        off(ScreenOff::ModelCantSee),
        "a new connection gets the state again"
    );
    assert!(output.is_empty());
    brain.close().await;
}

#[tokio::test]
async fn a_screen_hold_repeating_a_voice_holds_gesture_adds_no_part_to_it() {
    let scratch = Scratch::new("hold-screen-repeat");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let (_rig, _sol, _luna) = sees_on_sol(&mut brain);
    let voice = ControlToBrain::VoiceHoldRecognized {
        gesture: KEY.gesture,
        thread: None,
        selection: 1,
    };
    brain.hold_control(&voice, &mut link).unwrap();
    while output.try_recv().is_ok() {}
    assert!(
        brain
            .holds
            .pending
            .as_ref()
            .is_some_and(|hold| hold.key == KEY)
    );
    let screen = ControlToBrain::HoldRecognized {
        gesture: KEY.gesture,
        thread: None,
        selection: 1,
        pointer: kumochi_protocol::geometry::PhysicalPoint([5, 5]),
        lease: LEASE,
        t_qpc: 1,
    };
    assert!(brain.hold_control(&screen, &mut link).unwrap());
    assert_eq!(
        control(&mut output),
        ControlToShell::CaptureFailed {
            capture_for: kumochi_protocol::message::CaptureFor::Gesture(KEY.gesture),
            reason: kumochi_protocol::message::CaptureFailure::Unavailable,
        }
    );
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::None
    ));
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn the_end_of_a_capture_releases_the_lease_on_its_own_connection() {
    use crate::context::ScreenClass;
    use crate::screen::Generation;
    use kumochi_protocol::message::{CaptureFailure, CaptureFor};
    let scratch = Scratch::new("hold-screen-lease");
    let mut brain = test_support::brain(&scratch, "");
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(super::super::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let capture_for = CaptureFor::Gesture(KEY.gesture);
    let old = HoldKey {
        epoch: KEY.epoch + 1,
        ..KEY
    };
    let event = |key, generation, outcome| {
        Event::Screen(ScreenEvent {
            key,
            generation,
            outcome,
        })
    };
    let cases = [
        (
            event(KEY, Some(Generation(0)), Outcome::Held),
            Some(ControlToShell::CaptureDone { capture_for }),
        ),
        (
            event(KEY, None, Outcome::Closed(ScreenClass::NothingThere)),
            Some(ControlToShell::CaptureFailed {
                capture_for,
                reason: CaptureFailure::Unavailable,
            }),
        ),
        (
            event(
                KEY,
                Some(Generation(0)),
                Outcome::Closed(ScreenClass::Changed),
            ),
            None,
        ),
        (
            event(
                KEY,
                Some(Generation(0)),
                Outcome::Sealed(crate::screen::ScreenContext::fixture(JPEG)),
            ),
            None,
        ),
        (event(old, Some(Generation(0)), Outcome::Held), None),
    ];
    for (index, (event, lease)) in cases.into_iter().enumerate() {
        brain.hold_event(event, &mut link).unwrap();
        assert_eq!(
            output.try_recv().ok().map(|bytes| {
                kumochi_protocol::frame::decode::<ControlToShell>(
                    &bytes[kumochi_protocol::frame::HEADER_LEN..],
                )
                .unwrap()
            }),
            lease,
            "case {index}"
        );
    }
    recording(&mut brain);
    let hold = brain.holds.pending.as_ref().unwrap();
    hold.cancel.store(true, Ordering::SeqCst);
    brain
        .hold_event(event(KEY, Some(Generation(0)), Outcome::Held), &mut link)
        .unwrap();
    assert_eq!(
        control(&mut output),
        ControlToShell::CaptureDone { capture_for },
        "a cancelled hold's lease too"
    );
    brain.holds.lost();
    brain.close().await;
}

fn control(output: &mut mpsc::Receiver<Vec<u8>>) -> ControlToShell {
    let bytes = output.try_recv().unwrap();
    kumochi_protocol::frame::decode(&bytes[kumochi_protocol::frame::HEADER_LEN..]).unwrap()
}

#[tokio::test]
async fn turning_screen_holds_off_closes_the_pending_part_and_withdraws_the_runs_image() {
    use crate::context::{Jpeg, Screen};
    let scratch = Scratch::new("hold-screen-off");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let switched = |brain: &mut Brain, on: bool| {
        let pending = brain.config.stage(SettingsOp::SetScreenHolds(on)).unwrap();
        pending.write().unwrap();
        brain
            .settings_done((0, None, None, Ok((pending, Ok(())))), None)
            .unwrap();
    };
    recording(&mut brain);
    let snapshot = brain
        .holds
        .pending
        .as_ref()
        .unwrap()
        .snapshot
        .clone()
        .unwrap();
    let image = Screen::Image(Jpeg::new(JPEG.to_vec()).unwrap());
    brain
        .coordinator
        .submit_hold(snapshot, "words".into(), Some(image))
        .unwrap();
    brain.holds.pending.as_mut().unwrap().screen = ready();
    switched(&mut brain, false);
    assert!(matches!(
        brain.holds.pending.as_ref().unwrap().screen,
        ScreenPart::Discarded
    ));
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::Withdrawn)
    ));
    brain.holds.pending.as_mut().unwrap().screen = ready();
    switched(&mut brain, false);
    assert!(
        matches!(
            brain.holds.pending.as_ref().unwrap().screen,
            ScreenPart::Ready(_)
        ),
        "only turning it off closes a part"
    );
    brain.holds.lost();
    brain.close().await;
}
