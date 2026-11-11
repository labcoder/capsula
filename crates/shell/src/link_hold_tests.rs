use super::super::{Batch, DataOut, EndSession};
use super::*;
use crate::platform::microphone::{MicError, Mode};
use kumochi_protocol::message::{
    AccountOp, ConnectionKind, ConnectionOp, Effort, GestureId, HoldStatus, MicPcm, ModelChoice,
    ModelSlug, SettingsOp, ThreadOp,
};
use kumochi_protocol::voice::VoicePart;
use std::sync::atomic::Ordering;

#[test]
fn data_writer_scrubs_serialized_pcm_after_success() {
    let packet = private_pcm();
    let mut encoded = zeroize::Zeroizing::new(Vec::new());
    let mut bytes = Vec::new();
    super::super::write_data_frame(&mut bytes, &packet, &mut encoded).expect("written");
    let Some(DataToBrain::MicPcm(packet)) =
        frame::read(&mut bytes.as_slice(), Pipe::Data).expect("valid frame")
    else {
        panic!("one PCM packet");
    };
    assert_eq!(packet.gesture, GestureId(7));
    assert_eq!(packet.seq, 0);
    assert_eq!(packet.rate, 16_000);
    assert!(packet.last);
    assert_eq!(packet.samples, [17, -93, 405]);
    assert!(
        encoded.is_empty(),
        "private bytes survived a successful write"
    );
}

#[test]
fn data_writer_scrubs_serialized_pcm_after_write_failure() {
    let packet = private_pcm();
    for available in [0, 3] {
        let mut encoded = zeroize::Zeroizing::new(Vec::new());
        let mut bytes = [0; 3];
        let mut writer = std::io::Cursor::new(&mut bytes[..available]);
        let result = super::super::write_data_frame(&mut writer, &packet, &mut encoded);
        assert!(matches!(result, Err(frame::FrameError::Io(error))
            if error.kind() == std::io::ErrorKind::WriteZero));
        assert_eq!(
            writer.position(),
            u64::try_from(available).expect("small fixture")
        );
        assert!(encoded.is_empty(), "private bytes survived a failed write");
    }
}

#[test]
fn data_writer_scrubs_serialized_text_after_encode_failure() {
    let packet = DataToBrain::TextSubmit {
        turn: 1,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread: None,
        text: "x".repeat(Pipe::Data.max_payload()).into(),
    };
    let mut encoded = zeroize::Zeroizing::new(Vec::new());
    let mut bytes = Vec::new();
    let result = super::super::write_data_frame(&mut bytes, &packet, &mut encoded);
    assert!(matches!(
        result,
        Err(frame::FrameError::TooLarge { max: 262_144, .. })
    ));
    assert!(bytes.is_empty());
    assert!(
        encoded.is_empty(),
        "private bytes survived an encoding refusal"
    );
}

fn private_pcm() -> DataToBrain {
    DataToBrain::MicPcm(MicPcm {
        gesture: GestureId(7),
        seq: 0,
        rate: 16_000,
        last: true,
        samples: vec![17, -93, 405],
    })
}

#[test]
fn any_session_owner_ending_revokes_only_its_own_live_flag() {
    let old = Session::detached(1, Incarnation(1));
    let current = Session::detached(2, Incarnation(1));
    let writer = EndSession(old.alive());
    let delayed_reader = writer.clone();
    drop(writer);
    assert!(!old.alive().load(Ordering::Acquire));
    assert!(current.alive().load(Ordering::Acquire));
    drop(delayed_reader);
    assert!(current.alive().load(Ordering::Acquire));
}

#[test]
fn control_and_data_eof_revoke_without_dispatching_ui_events() {
    for data_loss in [false, true] {
        let link = attended(if data_loss {
            "hold-data-eof"
        } else {
            "hold-control-eof"
        });
        let session = connected(link.events.recv_blocking().expect("connected"));
        assert_eq!(link.handle.link_generation(), session.generation());
        if data_loss {
            drop(link.data);
        } else {
            drop(link.control);
        }
        let deadline = Instant::now() + PATIENCE;
        while session.alive().load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!session.alive().load(Ordering::Acquire));
        assert!(link.link.join().expect("joined"));
    }
}

#[test]
fn terminal_hold_status_revokes_before_ui_and_old_sessions_cannot_revoke_new_input() {
    let link = attended("hold-terminal");
    let session = connected(link.events.recv_blocking().expect("connected"));
    let gate = link.handle.microphone_gate();
    let token = gate.arm(GestureId(1)).expect("armed");
    link.handle.track_input(GestureId(1));
    frame::write(
        &mut &link.control,
        &ControlToShell::VoiceHoldStatus {
            gesture: GestureId(1),
            status: HoldStatus::Cancelled,
        },
        Pipe::Control,
    )
    .expect("terminal");
    let deadline = Instant::now() + PATIENCE;
    while link.handle.input_is(GestureId(1)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!link.handle.input_is(GestureId(1)));
    assert_eq!(
        token.mode(&session.alive(), Instant::now()),
        Err(MicError::Cancelled)
    );
    assert!(matches!(
        link.events.recv_blocking().expect("status"),
        LinkEvent::VoiceHold {
            generation: 1,
            gesture: GestureId(1),
            status: HoldStatus::Cancelled,
        }
    ));
    token.discard_unstarted().expect("retired");
    let newer = gate.arm(GestureId(2)).expect("next input");
    link.handle.track_input(GestureId(2));
    link.handle.shared.generations.store(2, Ordering::Release);
    frame::write(
        &mut &link.control,
        &ControlToShell::VoiceHoldStatus {
            gesture: GestureId(2),
            status: HoldStatus::Empty,
        },
        Pipe::Control,
    )
    .expect("old session terminal");
    assert!(matches!(
        link.events.recv_blocking().expect("old status"),
        LinkEvent::VoiceHold {
            generation: 1,
            gesture: GestureId(2),
            status: HoldStatus::Empty,
        }
    ));
    assert!(link.handle.input_is(GestureId(2)));
    assert_eq!(
        newer.mode(&session.alive(), Instant::now()),
        Ok(Mode::Preparing)
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn recorded_choice_mutations_cancel_before_queueing_and_away_back_still_changes_revision() {
    let choice = |slug| ModelChoice {
        model: ModelSlug::new(slug).expect("model"),
        effort: Effort::Medium,
        connection: ConnectionKind::ChatGpt,
    };
    let mutations = [
        ControlToBrain::Settings(SettingsOp::SetDefaultModel(choice("away"))),
        ControlToBrain::Settings(SettingsOp::SetDefaultModel(choice("original"))),
        ControlToBrain::ThreadOp(ThreadOp::SetModel {
            thread: ThreadId(1),
            choice: choice("other"),
        }),
        ControlToBrain::AccountOp(AccountOp::SignOut),
        ControlToBrain::Connection(ConnectionOp::RemoveAnthropicKey),
        ControlToBrain::Settings(SettingsOp::RemoveVoice(VoicePart::SpeechInput)),
    ];
    let mut link = LinkHandle::detached();
    let queue = link.record_controls();
    let session = Session::detached(1, Incarnation(1));
    let token = link.microphone_gate().arm(GestureId(1)).expect("armed");
    let captured = link.choice_revision();
    for (index, message) in mutations.into_iter().enumerate() {
        link.track_input(GestureId(1));
        assert!(link.send_control(&session, message.clone()));
        assert_eq!(
            queue.try_recv(),
            Ok(ControlToBrain::GestureCancel {
                gesture: GestureId(1)
            })
        );
        assert_eq!(queue.try_recv(), Ok(message));
        assert!(!link.input_is(GestureId(1)));
        assert_eq!(
            link.choice_revision(),
            captured + u64::try_from(index).expect("index") + 1
        );
    }
    assert_eq!(
        token.mode(&session.alive(), Instant::now()),
        Err(MicError::Cancelled)
    );
    assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
    assert!(link.send_control(&session, ControlToBrain::Pong));
    assert_eq!(link.choice_revision(), captured + 6);
}

#[test]
fn choice_revision_exhaustion_cancels_and_refuses_instead_of_wrapping() {
    let mut link = LinkHandle::detached();
    let queue = link.record_controls();
    let session = Session::detached(1, Incarnation(1));
    let token = link.microphone_gate().arm(GestureId(1)).expect("armed");
    link.track_input(GestureId(1));
    link.shared.choice.store(u64::MAX, Ordering::Release);
    for _ in 0..2 {
        assert!(!link.send_control(&session, ControlToBrain::AccountOp(AccountOp::SignOut)));
        assert_eq!(link.choice_revision(), u64::MAX);
        assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
    }
    assert!(!session.alive().load(Ordering::Acquire));
    assert_eq!(
        token.mode(&std::sync::atomic::AtomicBool::new(true), Instant::now()),
        Err(MicError::Cancelled)
    );
}

#[test]
fn hotkey_delivery_can_mark_escape_after_the_raw_edge_already_cancelled_input() {
    let link = LinkHandle::detached();
    link.track_input(GestureId(1));
    let escaped = link.cancel_input_gesture();
    assert!(!link.input_is(GestureId(1)));
    assert_eq!(link.cancel_input_gesture(), None);
    link.track_input(GestureId(2));
    assert_eq!(escaped, Some(GestureId(1)));
    assert!(link.input_is(GestureId(2)));
}

#[test]
fn matching_clear_and_escape_cancel_preserve_cancel_stop_order() {
    let mut link = LinkHandle::detached();
    let queue = link.record_controls();
    let session = Session::detached(1, Incarnation(1));
    link.track_input(GestureId(9));
    link.clear_input(GestureId(8));
    assert!(link.input_is(GestureId(9)));
    assert_eq!(link.cancel_input_gesture(), Some(GestureId(9)));
    assert_eq!(link.cancel_input_gesture(), None);
    assert!(link.send_control(
        &session,
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(9)
        }
    ));
    let stop = ControlToBrain::Stop {
        run: RunId(3),
        revealed: Vec::new(),
    };
    assert!(link.send_control(&session, stop.clone()));
    assert_eq!(
        queue.try_recv(),
        Ok(ControlToBrain::GestureCancel {
            gesture: GestureId(9)
        })
    );
    assert_eq!(
        queue.try_recv(),
        Ok(ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(9)
        })
    );
    assert_eq!(queue.try_recv(), Ok(stop));
    assert_eq!(queue.try_recv(), Err(async_channel::TryRecvError::Empty));
    link.track_input(GestureId(10));
    link.clear_input(GestureId(10));
    assert!(!link.input_is(GestureId(10)));
}

#[test]
fn pcm_never_counts_as_a_typed_turn_even_at_the_count_limit() {
    let link = attended("hold-pcm-count");
    let session = connected(link.events.recv_blocking().expect("connected"));
    let packet = || {
        DataToBrain::MicPcm(MicPcm {
            gesture: GestureId(1),
            seq: 0,
            rate: 48_000,
            last: false,
            samples: vec![23; 960],
        })
    };
    assert!(link.handle.send_data(&session, packet()));
    {
        let outbox = link.handle.shared.outbox.lock().expect("outbox");
        let outbox = outbox.as_ref().expect("connected");
        assert_eq!(outbox.submits.load(Ordering::Acquire), 0);
        outbox.submits.store(u32::MAX, Ordering::Release);
    }
    assert!(link.handle.send_data(&session, packet()));
    assert_eq!(
        link.handle
            .shared
            .outbox
            .lock()
            .expect("outbox")
            .as_ref()
            .expect("connected")
            .submits
            .load(Ordering::Acquire),
        u32::MAX
    );
    assert!(!link.handle.send_data(
        &session,
        DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: None,
            text: "not queued".into(),
        }
    ));
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn outer_link_and_queue_diagnostics_never_contain_text_or_sample_sentinels() {
    let marker = "PRIVATE_LINK_SENTINEL";
    let byte_list = format!("{:?}", marker.as_bytes());
    let audio = vec![12345_i16; 960];
    let sample_list = format!("{audio:?}");
    let event = LinkEvent::Data {
        generation: 1,
        message: DataToShell::AssistantText {
            run: RunId(1),
            delta: marker.into(),
        },
        bytes: 64,
    };
    let batch = Batch::Plain(vec![event]);
    let text = DataOut::Submit(DataToBrain::TextSubmit {
        turn: 1,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread: None,
        text: marker.into(),
    });
    let pcm = DataOut::Submit(DataToBrain::MicPcm(MicPcm {
        gesture: GestureId(1),
        seq: 0,
        rate: 48_000,
        last: false,
        samples: audio,
    }));
    for rendered in [
        format!("{batch:?}"),
        format!("{text:?}"),
        format!("{pcm:?}"),
    ] {
        assert!(!rendered.contains(marker));
        assert!(!rendered.contains(&byte_list));
        assert!(!rendered.contains("12345"));
        assert!(!rendered.contains(&sample_list));
    }
}
