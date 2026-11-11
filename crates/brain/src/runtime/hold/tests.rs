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
    });
    brain.holds.debt = Some(KEY);
    brain
        .holds
        .gate
        .active
        .store(KEY.gesture.0, Ordering::SeqCst);
}

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
