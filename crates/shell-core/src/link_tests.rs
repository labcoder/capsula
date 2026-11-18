use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Deleted, ErrorKey, Incarnation,
    IssueSession, Phase, RunId, RunOrigin, ThreadId, UserError,
};

use super::fixture::*;
use super::{Connection, Link, LinkEvent, LinkHandle, Session, Shared, start, welcome};
use crate::platform::PipeClient;
use crate::runs::{Refused, Runs, Shown};
use crate::threads::Threads;
use kumochi_protocol::message::{
    EventId, PageEvent, PageEventKind, PageOf, ThreadCursor, ThreadKind, ThreadRow,
};

#[test]
fn correlation_keys_cross_real_control_frames_into_the_runs_owner() {
    use kumochi_protocol::message::GestureId;

    let link = attended("run-correlation");
    let events = patient(link.events);
    let session = connected(next(&events));
    let mut runs = ready_runs(&session);
    runs.submit("older", |_| true).expect("first turn");
    runs.submit("second", |_| true).expect("second turn");
    for (run, phase, origin) in [
        (
            10,
            Phase::Listening,
            RunOrigin::Hold {
                gesture: GestureId(7),
            },
        ),
        (11, Phase::Thinking, RunOrigin::Typed { turn: 2 }),
    ] {
        frame::write(
            &mut &link.control,
            &run_state(run, ThreadId(1), phase, Some(origin)),
            Pipe::Control,
        )
        .expect("state written");
        let LinkEvent::RunState {
            generation,
            run: received_run,
            thread,
            phase: received_phase,
            error,
            origin: received_origin,
        } = next(&events)
        else {
            panic!("identified run state");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(received_run, RunId(run));
        assert_eq!(thread, ThreadId(1));
        assert_eq!(received_phase, phase);
        assert_eq!(error, None);
        assert_eq!(received_origin, Some(origin));
        let applied = runs.apply_state(received_run, thread, received_phase, received_origin);
        assert!(applied.visible);
        assert_eq!(applied.stop, None);
        assert_eq!(runs.kept(), [6]);
    }
    frame::write(
        &mut &link.control,
        &ControlToShell::TurnRefused {
            turn: 1,
            thread: Some(ThreadId(1)),
        },
        Pipe::Control,
    )
    .expect("refusal written");
    let LinkEvent::TurnRefused {
        generation,
        turn,
        thread,
    } = next(&events)
    else {
        panic!("identified refusal");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(turn, 1);
    assert_eq!(thread, Some(ThreadId(1)));
    runs.refused(turn, thread);
    let (applied, _) = runs.apply_run_state(
        RunId(11),
        ThreadId(1),
        Phase::Failed,
        Some(ErrorKey::SessionEnded.into()),
        None,
    );
    assert_eq!(applied.restore.as_deref(), Some("second"));
    assert_eq!(runs.kept(), Vec::<usize>::new());
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn voice_status_crosses_the_real_control_pipe_with_its_connection_generation() {
    use kumochi_protocol::message::{PartState, VoiceView};

    let link = attended("voice-status");
    let events = patient(link.events);
    let session = connected(next(&events));
    for speech_input in [
        PartState::Ready,
        PartState::Unloaded,
        PartState::NotInstalled,
    ] {
        frame::write(
            &mut &link.control,
            &ControlToShell::Voice(VoiceView {
                speech_input,
                spoken_replies: PartState::NotInstalled,
            }),
            Pipe::Control,
        )
        .expect("voice written");
        let LinkEvent::Voice {
            generation,
            speech_input: received,
        } = next(&events)
        else {
            panic!("voice status");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(received, speech_input);
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn capture_terminals_cross_the_real_control_pipe_with_reader_generation() {
    use kumochi_protocol::message::{CaptureFailure, CaptureFor, GestureId};
    let link = attended("capture-terminals");
    let events = patient(link.events);
    let session = connected(next(&events));
    for capture_for in [CaptureFor::Gesture(GestureId(7)), CaptureFor::Turn(7)] {
        frame::write(
            &mut &link.control,
            &ControlToShell::CaptureDone { capture_for },
            Pipe::Control,
        )
        .expect("done written");
        let LinkEvent::CaptureDone {
            generation,
            capture_for: received,
        } = next(&events)
        else {
            panic!("capture done");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(received, capture_for);
        for reason in [
            CaptureFailure::ExclusionUnavailable,
            CaptureFailure::Cancelled,
            CaptureFailure::Unavailable,
        ] {
            frame::write(
                &mut &link.control,
                &ControlToShell::CaptureFailed {
                    capture_for,
                    reason,
                },
                Pipe::Control,
            )
            .expect("failure written");
            let LinkEvent::CaptureFailed {
                generation,
                capture_for: received,
                reason: category,
            } = next(&events)
            else {
                panic!("capture failed");
            };
            assert_eq!(generation, session.generation());
            assert_eq!(received, capture_for);
            assert_eq!(category, reason);
        }
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn screen_hold_messages_cross_the_real_control_pipe_with_reader_generation() {
    use kumochi_protocol::message::{
        GestureId, HeldScreen, ModelName, ScreenClass, ScreenHolds, ScreenOff,
    };
    let link = attended("screen-holds");
    let events = patient(link.events);
    let session = connected(next(&events));
    for state in [
        ScreenHolds::On {
            model: ModelName::new("GPT-5.5").expect("valid"),
        },
        ScreenHolds::Off(ScreenOff::TurnedOff),
    ] {
        frame::write(
            &mut &link.control,
            &ControlToShell::ScreenHolds(state.clone()),
            Pipe::Control,
        )
        .expect("state written");
        let LinkEvent::ScreenHolds {
            generation,
            state: received,
        } = next(&events)
        else {
            panic!("screen holds");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(received, state);
    }
    for outcome in [
        HeldScreen::Attached,
        HeldScreen::NotIncluded(ScreenClass::Changed),
    ] {
        frame::write(
            &mut &link.control,
            &ControlToShell::HoldScreen {
                gesture: GestureId(7),
                outcome,
            },
            Pipe::Control,
        )
        .expect("outcome written");
        let LinkEvent::HoldScreen {
            generation,
            gesture,
            outcome: received,
        } = next(&events)
        else {
            panic!("hold screen");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(gesture, GestureId(7));
        assert_eq!(received, outcome);
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn lapse_notification_is_only_session_fenced_bounded_queue_acceptance() {
    use kumochi_protocol::message::LeaseGeneration;
    let (_brain, pipe) = pair("lapse-outbox");
    let link = LinkHandle::detached();
    let session = Session::detached(7, Incarnation(1));
    let (control, received) = async_channel::bounded(1);
    let (data, _data_received) = async_channel::bounded(1);
    *link.shared.outbox.lock().expect("outbox lock") = Some(super::Outbox {
        generation: 7,
        control,
        data,
        pipe: Arc::new(pipe),
        submits: std::sync::atomic::AtomicU32::new(0),
    });
    assert!(link.send_control(&session, ControlToBrain::Pong));
    assert!(!link.send_control(
        &session,
        ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(1)
        }
    ));
    assert_eq!(
        received.try_recv().expect("original queue item"),
        ControlToBrain::Pong
    );
    let old = Session::detached(6, Incarnation(1));
    assert!(!link.send_control(
        &old,
        ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(1)
        }
    ));
    assert!(received.is_empty());
    assert!(link.send_control(
        &session,
        ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(1)
        }
    ));
    assert_eq!(
        received.try_recv().expect("queued notification"),
        ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(1)
        }
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one real pipe connection exercises every typed scene frame family"
)]
fn typed_scene_frames_keep_their_session_and_exact_credit_on_real_pipes() {
    use kumochi_protocol::message::{SceneDescriptorV1, SceneHeader, SceneId, SceneSnapshotChunk};
    use kumochi_scene::{Record, SceneEvent, StopCut, parse_record, text::Bounded};

    let link = attended("typed-scenes");
    let events = patient(link.events);
    let session = connected(next(&events));
    let scene = SceneId(7);
    let descriptors = Bounded::new(vec![
        SceneDescriptorV1::new(scene, 1, 1, 1).expect("descriptor"),
    ])
    .expect("one scene");
    frame::write(
        &mut &link.control,
        &ControlToShell::ScenesV1 {
            scenes: descriptors.clone(),
        },
        Pipe::Control,
    )
    .expect("membership written");
    let LinkEvent::ScenesV1 { generation, scenes } = next(&events) else {
        panic!("typed membership")
    };
    assert_eq!(generation, session.generation());
    assert_eq!(scenes, descriptors);
    let Record::Begin(begin) = parse_record(
        br#"{"begin":{"v":1,"scene":"private-scene","surface":"overlay","alt":"private-alt"}}"#,
    )
    .expect("header") else {
        panic!("begin")
    };
    let Record::Step(step) = parse_record(br#"{"step":{"id":"private-step"}}"#).expect("step")
    else {
        panic!("step")
    };
    let header = SceneHeader {
        begin,
        layout_version: 1,
    };
    let frames = [
        DataToShell::SceneBegin {
            scene,
            header: header.clone(),
        },
        DataToShell::SceneStepCommit {
            run: RunId(3),
            scene,
            index: 0,
            step: step.clone(),
        },
        DataToShell::SceneSnapshotV1 {
            chunk: SceneSnapshotChunk {
                scene,
                revision: 1,
                ordinal: 0,
                header: Some(header),
                events: Bounded::new(vec![SceneEvent::Step {
                    revision: 1,
                    step: Arc::new(step),
                }])
                .expect("event"),
                last: true,
            },
        },
        DataToShell::SceneCutApplied {
            scene,
            revision: 1,
            cut: StopCut {
                activated: 1,
                revealed: Bounded::default(),
            },
        },
        DataToShell::SceneEnd {
            run: RunId(3),
            scene,
            complete: true,
        },
        DataToShell::ScenesRemoved {
            scenes: vec![scene],
        },
    ];
    for sent in frames {
        frame::write(&mut &*link.data, &sent, Pipe::Data).expect("typed frame written");
        let LinkEvent::SceneData {
            generation,
            frame: message,
            bytes,
        } = next(&events)
        else {
            panic!("typed scene data, protected from its read")
        };
        assert_eq!(generation, session.generation());
        assert_eq!(*message, sent);
        assert_eq!(
            usize::try_from(bytes).expect("frame size"),
            frame::encode(&sent, Pipe::Data).expect("frame").len()
        );
        session.grant(bytes);
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("credit"),
            Some(DataToBrain::Credit { bytes })
        );
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn settings_frames_keep_their_connection_generation_and_authoritative_view() {
    use kumochi_protocol::message::{AssistantName, SettingsView};
    let link = attended("settings-view");
    let events = patient(link.events);
    let session = connected(next(&events));
    let expected = SettingsView {
        assistant_name: AssistantName::new("Cloud").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(9),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    };
    frame::write(
        &mut &link.control,
        &ControlToShell::Settings {
            answer: false,
            view: expected.clone(),
        },
        Pipe::Control,
    )
    .expect("written");
    let LinkEvent::Settings {
        generation,
        view,
        answer,
    } = next(&events)
    else {
        panic!("settings")
    };
    assert_eq!(generation, session.generation());
    assert!(!answer);
    assert_eq!(view, expected);
    frame::write(
        &mut &link.control,
        &ControlToShell::Settings {
            answer: true,
            view: expected.clone(),
        },
        Pipe::Control,
    )
    .expect("written");
    let LinkEvent::Settings {
        generation: answered_generation,
        view: answered_view,
        answer: true,
    } = next(&events)
    else {
        panic!("a settings answer keeps its answer flag")
    };
    assert_eq!(answered_generation, session.generation());
    assert_eq!(answered_view, expected);
    let mut runs = Runs::default();
    runs.connected(generation);
    runs.settings(view.main_thread);
    runs.submit("home", |turn| link.handle.send_data(&session, turn))
        .expect("sent");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(ThreadId(9)),
            text: "home".into()
        })
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn page_frames_decode_on_their_session_and_earn_exact_credit_when_applied_or_dropped() {
    let link = attended("pages");
    let events = patient(link.events);
    let session = connected(next(&events));
    let mut threads = Threads::default();
    threads.show();
    threads.connected();
    let request = threads.requests().remove(0);
    assert!(link.handle.send_data(&session, request.clone()));
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(request)
    );
    let list = DataToShell::ThreadList {
        request: 1,
        threads: vec![ThreadRow {
            id: ThreadId(3),
            title: "a private title".to_owned(),
            updated_ms: 9,
            kind: ThreadKind::Thread,
            choice: None,
        }],
        next: Some(ThreadCursor {
            id: ThreadId(3),
            updated_ms: 9,
            rank_bits: 0,
        }),
    };
    let page = DataToShell::Page {
        request: 99,
        thread: ThreadId(3),
        events: vec![PageEvent {
            id: EventId(4),
            run: Some(RunId(5)),
            kind: PageEventKind::User,
            text: "private text".to_owned(),
            cut: true,
        }],
        before: Some(EventId(4)),
    };
    for sent in [list, page] {
        frame::write(&mut &*link.data, &sent, Pipe::Data).expect("page written");
        let LinkEvent::Data {
            generation,
            message,
            bytes,
        } = next(&events)
        else {
            panic!("data")
        };
        assert_eq!(generation, session.generation());
        assert_eq!(message, sent);
        assert_eq!(
            usize::try_from(bytes).expect("small"),
            frame::encode(&sent, Pipe::Data).expect("frame").len()
        );
        session.grant(threads.receive(message, bytes));
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("credit"),
            Some(DataToBrain::Credit { bytes })
        );
    }
    assert_eq!(threads.rows[0].id, ThreadId(3));
    assert!(
        threads.events.is_empty(),
        "the unsolicited event page was dropped, but credited"
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
    assert!(!link.handle.send_data(
        &session,
        DataToBrain::PageRequest {
            request: 3,
            page: PageOf::Threads {
                search: None,
                after: None
            }
        }
    ));
}

/// Bytes of text each data sample streams.
const TOTAL: usize = 960_000;

#[test]
fn ending_a_session_closes_its_real_pipes_and_retires_control_admission() {
    let link = attended("settings-cancel-full-fallback");
    let events = patient(link.events);
    let session = connected(next(&events));
    link.handle.end(&session);
    assert!(link.link.join().expect("the aborted session joins"));
    assert!(!link.handle.send_control(&session, ControlToBrain::Pong));
    assert!(link.handle.shared.outbox.lock().unwrap().is_none());
}

#[test]
fn the_last_reader_generation_is_usable_but_exhaustion_never_wraps() {
    let link = attended_from("last-generation", u64::MAX - 1);
    let events = patient(link.events);
    let session = connected(next(&events));
    assert_eq!(session.generation(), u64::MAX);
    drop(link.control);
    assert!(link.link.join().expect("last session joined"));

    let link = attended_from("exhausted-generation", u64::MAX);
    let events = patient(link.events);
    assert!(matches!(
        next(&events),
        LinkEvent::State(super::LinkState::Failed)
    ));
    assert!(!link.link.join().expect("exhausted session joined"));
    assert_eq!(
        link.handle
            .shared
            .generations
            .load(std::sync::atomic::Ordering::SeqCst),
        u64::MAX
    );
    assert!(!link.handle.send_control(
        &Session::detached(u64::MAX, Incarnation(1)),
        ControlToBrain::Pong
    ));
}

#[test]
fn scene_frames_lost_in_a_batch_or_the_queue_drop_on_the_dropper() {
    use crate::presenter::tests::{drained, hold_dropper};
    use kumochi_protocol::message::{SceneHeader, SceneId, SceneSnapshotChunk};
    use kumochi_scene::{Record, SceneEvent, deferred::Deferred, parse_record, text::Bounded};

    let Record::Step(step) = parse_record(br#"{"step":{"id":"private-step"}}"#).expect("step")
    else {
        panic!("a step record");
    };
    let link = attended("lost-scene-owner");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let Record::Begin(begin) = parse_record(
        br#"{"begin":{"v":1,"scene":"private-scene","surface":"overlay","alt":"private-alt"}}"#,
    )
    .expect("header") else {
        panic!("begin record");
    };
    let sent = DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene: SceneId(7),
            revision: 1,
            ordinal: 0,
            header: Some(SceneHeader {
                begin,
                layout_version: 1,
            }),
            events: Bounded::new(vec![SceneEvent::Step {
                revision: 1,
                step: Arc::new(step),
            }])
            .expect("one event"),
            last: true,
        },
    };
    frame::write(&mut &*link.data, &sent, Pipe::Data).expect("snapshot written");
    let event = next(&events);
    let LinkEvent::SceneData { frame, .. } = &event else {
        panic!("the data reader protects the scene frame");
    };
    let DataToShell::SceneSnapshotV1 { chunk } = &**frame else {
        panic!("snapshot data");
    };
    let SceneEvent::Step { step, .. } = &chunk.events[0] else {
        panic!("one step");
    };
    let step = Arc::clone(step);
    let queue = |frames: u16| {
        let (sender, receiver) = async_channel::unbounded();
        for ordinal in 0..frames {
            let chunk = SceneSnapshotChunk {
                scene: SceneId(7),
                revision: 1,
                ordinal,
                header: None,
                events: Bounded::new(vec![SceneEvent::Step {
                    revision: 1,
                    step: Arc::clone(&step),
                }])
                .expect("one event"),
                last: false,
            };
            let frame = DataToShell::SceneSnapshotV1 { chunk };
            sender
                .send_blocking(LinkEvent::SceneData {
                    generation: 1,
                    frame: Deferred::new(frame, 40),
                    bytes: 40,
                })
                .expect("queued");
        }
        (sender, receiver)
    };
    let release = hold_dropper();
    let (sender, receiver) = queue(1);
    sender
        .send_blocking(event)
        .expect("the actual producer's event");
    let first = receiver.recv_blocking().expect("an event");
    let batch = super::batch(first, &receiver);
    assert_eq!(batch.len(), 2, "the batch takes what was already queued");
    drop(batch);
    assert_eq!(
        Arc::strong_count(&step),
        3,
        "a batch whose update failed waits for the dropper"
    );
    let (sender, receiver) = queue(1);
    drop(sender);
    drop(receiver);
    assert_eq!(
        Arc::strong_count(&step),
        4,
        "a frame still queued when the channel's last handle goes waits for the dropper"
    );
    drop(release);
    assert!(drained(&step, 1), "the dropper destroys them");
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn a_lost_batch_moves_its_container_allocation_off_the_caller() {
    let release = crate::presenter::tests::hold_dropper();
    let batch = || {
        let (sender, receiver) = async_channel::unbounded();
        for _ in 1..super::EVENT_BATCH {
            sender
                .send_blocking(LinkEvent::Scenes(Vec::new()))
                .expect("queued metadata");
        }
        super::batch(
            LinkEvent::SceneData {
                generation: 1,
                frame: kumochi_scene::deferred::Deferred::new(
                    DataToShell::ScenesRemoved { scenes: Vec::new() },
                    0,
                ),
                bytes: 0,
            },
            &receiver,
        )
    };
    let guarded = batch();
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(guarded));
    assert_eq!(released, 0, "even the outer vector stays off the caller");

    let bare: Vec<_> = (0..super::EVENT_BATCH)
        .map(|_| LinkEvent::Scenes(Vec::new()))
        .collect();
    let bytes = bare.capacity() * size_of::<LinkEvent>();
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(bare));
    assert_eq!(
        released, bytes,
        "the pre-wrapper control frees its vector on the caller"
    );
    drop(release);
}

#[test]
fn a_text_only_batch_frees_its_container_on_the_caller() {
    let release = crate::presenter::tests::hold_dropper();
    let (_sender, receiver) = async_channel::unbounded();
    let batch = super::batch(
        LinkEvent::Data {
            generation: 1,
            message: text(1, ""),
            bytes: 40,
        },
        &receiver,
    );
    let bytes = batch.capacity() * size_of::<LinkEvent>();
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(batch));
    assert_eq!(released, bytes, "text needs no dropper allocation or send");
    drop(release);
}

#[test]
fn a_mixed_batch_retires_its_buffer_when_scene_data_arrives_last() {
    let release = crate::presenter::tests::hold_dropper();
    let (sender, receiver) = async_channel::unbounded();
    sender
        .send_blocking(LinkEvent::Data {
            generation: 1,
            message: text(2, ""),
            bytes: 40,
        })
        .expect("second text event");
    sender
        .send_blocking(LinkEvent::SceneData {
            generation: 1,
            frame: kumochi_scene::deferred::Deferred::new(
                DataToShell::ScenesRemoved { scenes: Vec::new() },
                0,
            ),
            bytes: 40,
        })
        .expect("scene event last");
    let batch = super::batch(
        LinkEvent::Data {
            generation: 1,
            message: text(1, ""),
            bytes: 40,
        },
        &receiver,
    );
    assert_eq!(batch.len(), 3);
    assert_eq!(receiver.len(), 0);
    assert!(matches!(&batch, super::Batch::Scene(_)));
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(batch));
    assert_eq!(released, 0, "the mixed batch's buffer leaves the caller");
    drop(release);
}

#[test]
fn a_full_batch_leaves_the_next_event_queued() {
    let (sender, receiver) = async_channel::unbounded();
    for index in 1..=256 {
        sender
            .send_blocking(LinkEvent::Data {
                generation: 1,
                message: text(index, ""),
                bytes: 40,
            })
            .expect("queued text event");
    }
    let batch = super::batch(
        LinkEvent::Data {
            generation: 1,
            message: text(0, ""),
            bytes: 40,
        },
        &receiver,
    );
    assert_eq!(batch.len(), 256);
    assert_eq!(receiver.len(), 1);
    let LinkEvent::Data {
        message: DataToShell::AssistantText { run, delta },
        ..
    } = receiver.try_recv().expect("the 257th event stays queued")
    else {
        panic!("the next text event");
    };
    assert_eq!(run, RunId(256));
    assert_eq!(delta, "");
    assert_eq!(receiver.len(), 0);
}

/// A link to no brain, with no speaker.
fn bare_link() -> Link {
    Link {
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
        speaker: None,
    }
}

fn text(run: u64, delta: &str) -> DataToShell {
    DataToShell::AssistantText {
        run: RunId(run),
        delta: delta.into(),
    }
}

#[test]
fn nothing_from_the_brain_comes_before_connected_and_frames_carry_their_size() {
    let link = attended("order");
    let sent = text(1, "early");
    frame::write(&mut &*link.data, &sent, Pipe::Data).expect("written");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let LinkEvent::Data { message, bytes, .. } = next(&events) else {
        panic!("data next");
    };
    assert_eq!(message, sent);
    let size = frame::encode(&sent, Pipe::Data).expect("encodes").len();
    assert_eq!(usize::try_from(bytes).expect("small"), size);
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn pings_are_answered_and_run_states_passed_on() {
    let link = attended("ping");
    let events = patient(link.events);
    let session = connected(next(&events));
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    let pong = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control);
    assert_eq!(pong.expect("read"), Some(ControlToBrain::Pong));
    let state = ControlToShell::RunState {
        origin: None,
        run: RunId(2),
        thread: ThreadId(3),
        phase: Phase::Failed,
        error: Some(UserError::from(ErrorKey::SignInNeeded)),
    };
    frame::write(&mut &link.control, &state, Pipe::Control).expect("state");
    assert!(matches!(
        next(&events),
        LinkEvent::RunState {
            generation,
            origin: None,
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Failed,
            error: Some(error),
        } if generation == session.generation() && error.key() == ErrorKey::SignInNeeded
    ));
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn model_write_results_are_accepted_without_ending_the_link() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

    let link = attended("model-result");
    let events = patient(link.events);
    let _session = connected(next(&events));
    for applied in [false, true] {
        frame::write(
            &mut &link.control,
            &ControlToShell::ThreadModelSet {
                thread: ThreadId(3),
                choice: ModelChoice {
                    model: ModelSlug::new("private-model").expect("valid"),
                    effort: Effort::High,
                    connection: ConnectionKind::ChatGpt,
                },
                applied,
            },
            Pipe::Control,
        )
        .expect("model result");
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control)
            .expect("ping after result");
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
            Some(ControlToBrain::Pong)
        );
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one existing pipe connection checks no-op controls between text and scene frames"
)]
fn legacy_no_visual_controls_are_no_ops_before_more_valid_traffic() {
    use kumochi_protocol::message::{SceneHeader, SceneId};
    use kumochi_scene::{Record, parse_record};

    let link = attended("no-visual-advisory");
    let events = patient(link.events);
    let session = connected(next(&events));
    for run in [RunId(0), RunId(2), RunId(u64::MAX), RunId(2)] {
        frame::write(
            &mut &link.control,
            &ControlToShell::NoVisual { run },
            Pipe::Control,
        )
        .expect("legacy advisory");
    }
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("pong"),
        Some(ControlToBrain::Pong)
    );
    frame::write(
        &mut &link.control,
        &run_state(2, ThreadId(3), Phase::Answering, None),
        Pipe::Control,
    )
    .expect("normal run state");
    assert!(matches!(
        next(&events),
        LinkEvent::RunState {
            generation,
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Answering,
            error: None,
            origin: None
        } if generation == session.generation()
    ));
    let text = DataToShell::AssistantText {
        run: RunId(2),
        delta: "still connected".into(),
    };
    frame::write(&mut &*link.data, &text, Pipe::Data).expect("text frame");
    let LinkEvent::Data {
        generation,
        message,
        bytes,
    } = next(&events)
    else {
        panic!("only the normal text event");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(message, text);
    assert_eq!(
        usize::try_from(bytes).expect("text frame size"),
        frame::encode(&text, Pipe::Data)
            .expect("text encoding")
            .len()
    );
    session.grant(bytes);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("text credit"),
        Some(DataToBrain::Credit { bytes })
    );
    let Record::Begin(begin) =
        parse_record(br#"{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"}}"#)
            .expect("typed begin")
    else {
        panic!("begin");
    };
    let Record::Step(step) = parse_record(br#"{"step":{"id":"s1"}}"#).expect("typed step") else {
        panic!("step");
    };
    for sent in [
        DataToShell::SceneBegin {
            scene: SceneId(7),
            header: SceneHeader {
                begin,
                layout_version: 1,
            },
        },
        DataToShell::SceneStepCommit {
            run: RunId(2),
            scene: SceneId(7),
            index: 0,
            step,
        },
        DataToShell::SceneEnd {
            run: RunId(2),
            scene: SceneId(7),
            complete: true,
        },
    ] {
        frame::write(
            &mut &link.control,
            &ControlToShell::NoVisual { run: RunId(2) },
            Pipe::Control,
        )
        .expect("repeated advisory");
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("pong"),
            Some(ControlToBrain::Pong)
        );
        frame::write(&mut &*link.data, &sent, Pipe::Data).expect("scene frame");
        let LinkEvent::SceneData {
            generation,
            frame: message,
            bytes,
        } = next(&events)
        else {
            panic!("normal typed scene event, with no advisory event");
        };
        assert_eq!(generation, session.generation());
        assert_eq!(*message, sent);
        assert_eq!(
            usize::try_from(bytes).expect("size"),
            frame::encode(&sent, Pipe::Data).expect("frame").len()
        );
        session.grant(bytes);
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("scene credit"),
            Some(DataToBrain::Credit { bytes })
        );
    }
    let end = DataToShell::RunEnd { run: RunId(2) };
    frame::write(&mut &*link.data, &end, Pipe::Data).expect("ordinary run end");
    let LinkEvent::Data { message, bytes, .. } = next(&events) else {
        panic!("run end");
    };
    assert_eq!(message, end);
    assert_eq!(
        usize::try_from(bytes).expect("end frame size"),
        frame::encode(&end, Pipe::Data).expect("end encoding").len()
    );
    session.grant(bytes);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("end credit"),
        Some(DataToBrain::Credit { bytes })
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn no_visual_cannot_replace_the_authenticated_welcome() {
    let (control, shell_control) = pair("no-visual-before-welcome");
    frame::write(
        &mut &control,
        &ControlToShell::NoVisual { run: RunId(2) },
        Pipe::Control,
    )
    .expect("unexpected first frame");
    assert!(matches!(
        welcome(&shell_control),
        Err(super::LinkError::Rejected)
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one wire scenario covers state/refusal ordering and exact card credit"
)]
fn staged_issue_controls_keep_the_link_and_cards_return_exact_credit() {
    use kumochi_protocol::message::{
        ActionState, ContentHash, DraftId, GitHubView, IssueBody, IssueCard, IssueRefused,
        IssueTitle, StateVersion,
    };

    let link = attended("staged-issues");
    let events = patient(link.events);
    let session = connected(next(&events));
    for message in [
        ControlToShell::GitHub(GitHubView::Requesting),
        ControlToShell::ActionState {
            request: None,
            draft: DraftId(9),
            version: StateVersion(1),
            state: ActionState::Draft,
            approval: None,
        },
        ControlToShell::IssueRefused {
            request: 7,
            version: StateVersion(1),
            approval: None,
            draft: DraftId(9),
            reason: IssueRefused::NotAllowedNow,
        },
        ControlToShell::IssueUnavailable {
            request: 8,
            draft: DraftId(9),
            kind: kumochi_protocol::message::IssueUnavailableKind::Action,
        },
        ControlToShell::Ping,
    ] {
        frame::write(&mut &link.control, &message, Pipe::Control).expect("written");
    }
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(ControlToBrain::Pong)
    );
    let LinkEvent::GitHub { generation, view } = next(&events) else {
        panic!("GitHub view reaches its session-fenced consumer");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(view, kumochi_protocol::message::GitHubView::Requesting);
    let LinkEvent::ActionState {
        request,
        generation,
        draft,
        version,
        state,
        approval,
    } = next(&events)
    else {
        panic!("action notification");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(
        (draft, version, state, approval, request),
        (DraftId(9), StateVersion(1), ActionState::Draft, None, None)
    );
    let LinkEvent::IssueRefused {
        generation,
        request,
        draft,
        version,
        approval,
        reason,
    } = next(&events)
    else {
        panic!("correlated refusal reaches its session-fenced consumer");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(
        (request, draft, version, approval, reason),
        (
            7,
            DraftId(9),
            StateVersion(1),
            None,
            IssueRefused::NotAllowedNow
        )
    );
    let LinkEvent::IssueUnavailable {
        generation,
        request,
        draft,
        kind,
    } = next(&events)
    else {
        panic!("terminal storage failure reaches its session-fenced consumer");
    };
    assert_eq!(generation, session.generation());
    assert_eq!((request, draft), (8, DraftId(9)));
    assert_eq!(
        kind,
        kumochi_protocol::message::IssueUnavailableKind::Action
    );
    for kind in [
        kumochi_protocol::message::IssueUnavailableKind::ReviewNotSaved,
        kumochi_protocol::message::IssueUnavailableKind::ReviewCapacity,
        kumochi_protocol::message::IssueUnavailableKind::AttemptNotSaved,
    ] {
        frame::write(
            &mut &link.control,
            &ControlToShell::IssueUnavailable {
                request: 9,
                draft: DraftId(9),
                kind,
            },
            Pipe::Control,
        )
        .expect("typed terminal");
        let LinkEvent::IssueUnavailable {
            generation,
            request,
            draft,
            kind: received,
        } = next(&events)
        else {
            panic!("typed terminal reaches its consumer");
        };
        assert_eq!(
            (generation, request, draft, received),
            (session.generation(), 9, DraftId(9), kind)
        );
    }
    let card = DataToShell::IssueCard(IssueCard {
        draft: DraftId(9),
        thread: ThreadId(3),
        repo: None,
        account: None,
        title: IssueTitle::new("Fix layout").expect("title"),
        body: IssueBody::new("Private details").expect("body"),
        hash: ContentHash([0; 32]),
        approval: None,
        version: StateVersion(1),
        state: ActionState::Draft,
    });
    let size = frame::encode(&card, Pipe::Data).expect("frame").len();
    frame::write(&mut &*link.data, &card, Pipe::Data).expect("written");
    let LinkEvent::Data {
        generation,
        message,
        bytes,
    } = next(&events)
    else {
        panic!("only the card reaches the UI")
    };
    assert_eq!(generation, session.generation());
    assert_eq!(message, card);
    assert_eq!(usize::try_from(bytes).expect("small"), size);
    let mut runs = Runs::default();
    let credit = runs.apply_data(message, bytes);
    assert_eq!(credit, bytes);
    assert_eq!(runs.shown(), None);
    session.grant(credit);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(DataToBrain::Credit { bytes })
    );
    let DataToShell::IssueCard(card) = card else {
        panic!("card fixture");
    };
    let review = DataToShell::IssueReview {
        request: u64::MAX,
        card,
    };
    frame::write(&mut &*link.data, &review, Pipe::Data).expect("review data");
    let LinkEvent::Data {
        generation,
        message,
        bytes,
    } = next(&events)
    else {
        panic!("review stays on data");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(message, review);
    assert_eq!(
        usize::try_from(bytes).unwrap(),
        frame::encode(&review, Pipe::Data).unwrap().len()
    );
    session.grant(bytes);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
        Some(DataToBrain::Credit { bytes })
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn github_errors_keep_their_session_and_do_not_replace_generic_account_errors() {
    use kumochi_protocol::message::{Action, ErrorArg};
    let link = attended("github-errors");
    let events = patient(link.events);
    let session = connected(next(&events));
    let timeout = UserError::new(
        ErrorKey::SignInTimedOut,
        vec![ErrorArg::Seconds(5)],
        vec![Action::Retry],
    )
    .unwrap();
    for message in [
        ControlToShell::GitHubError(ErrorKey::GitHubUnavailable.into()),
        ControlToShell::Error(ErrorKey::AccountOffline.into()),
        ControlToShell::GitHubError(timeout.clone()),
        ControlToShell::Ping,
    ] {
        frame::write(&mut &link.control, &message, Pipe::Control).unwrap();
    }
    let LinkEvent::GitHubError { generation, error } = next(&events) else {
        panic!("tagged refusal")
    };
    assert_eq!(generation, session.generation());
    assert_eq!(error, ErrorKey::GitHubUnavailable.into());
    let LinkEvent::Error { generation, error } = next(&events) else {
        panic!("generic account error")
    };
    assert_eq!(generation, session.generation());
    assert_eq!(error, ErrorKey::AccountOffline.into());
    let LinkEvent::GitHubError { generation, error } = next(&events) else {
        panic!("tagged existing error")
    };
    assert_eq!(generation, session.generation());
    assert_eq!(error, timeout);
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
        Some(ControlToBrain::Pong)
    );
    assert_eq!(session.granted(), 0);
    drop(link.control);
    assert!(link.link.join().unwrap());
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
        None
    );
}

#[test]
fn a_github_error_cannot_replace_the_initial_welcome() {
    let (control, shell_control) = pair("github-error-before-welcome");
    control.set_timeout(Some(PATIENCE));
    shell_control.set_timeout(Some(PATIENCE));
    frame::write(
        &mut &control,
        &ControlToShell::GitHubError(ErrorKey::GitHubUnavailable.into()),
        Pipe::Control,
    )
    .unwrap();
    assert!(matches!(
        welcome(&shell_control),
        Err(super::LinkError::Rejected)
    ));
}

#[test]
fn the_ui_s_messages_go_out_on_their_pipes_and_credit_adds_up() {
    let link = attended("outbox");
    let events = patient(link.events);
    let session = connected(next(&events));
    let stop = ControlToBrain::Stop {
        run: RunId(3),
        revealed: vec![0],
    };
    assert!(link.handle.send_control(&session, stop.clone()));
    let read = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control);
    assert_eq!(read.expect("read"), Some(stop));
    let submit = DataToBrain::TextSubmit {
        turn: 1,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread: None,
        text: "typed".into(),
    };
    assert!(link.handle.send_data(&session, submit.clone()));
    session.grant(100);
    session.grant(28);
    let read = frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data);
    assert_eq!(read.expect("read"), Some(submit));
    let mut credit = 0;
    while credit < 128 {
        let read = frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data);
        let Ok(Some(DataToBrain::Credit { bytes })) = read else {
            panic!("credit, got {read:?}");
        };
        credit += bytes;
    }
    assert_eq!(credit, 128);
    drop(link.control);
    assert!(link.link.join().expect("joined"));
    assert!(
        !link.handle.send_control(&session, ControlToBrain::Pong),
        "not connected"
    );
}

#[test]
fn a_malformed_data_frame_ends_the_connection() {
    let link = attended("malformed");
    let events = patient(link.events);
    let session = connected(next(&events));
    // A three-byte payload naming no `DataToShell` variant.
    std::io::Write::write_all(&mut &*link.data, &[3, 0, 0, 0, 0xff, 0xff, 0xff]).expect("written");
    // The shell closes the control pipe too; this read times out if it doesn't.
    assert!(matches!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control),
        Ok(None)
    ));
    assert!(link.link.join().expect("joined"));
    assert!(
        !link.handle.send_control(&session, ControlToBrain::Pong),
        "not connected"
    );
}

#[test]
fn a_repeated_handshake_ends_the_connection() {
    let link = attended("rehandshake");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let welcome = ControlToShell::Welcome {
        incarnation: Incarnation(2),
        issue_epoch: 2,
    };
    frame::write(&mut &link.control, &welcome, Pipe::Control).expect("written");
    assert!(matches!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control),
        Ok(None)
    ));
    assert!(link.link.join().expect("joined"));
}

/// A connection's first frames in the order the brain sends them, which its tap test pins:
/// the handshake takes Welcome, and the link passes Settings and Scenes on around the account
/// view without ending the connection.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one handshake scenario checks server provenance and ordered first frames through the real link"
)]
fn the_brain_s_first_frames_with_the_account_view_keep_the_connection() {
    use kumochi_protocol::message::{AccountView, AssistantName, MaxFrameRate, SettingsView};
    let (control, shell_control) = pair("first-frames-control");
    let (_data, shell_data) = pair("first-frames-data");
    control.set_timeout(Some(PATIENCE));
    let view = SettingsView {
        assistant_name: AssistantName::new("Cloud").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(9),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    };
    for message in [
        ControlToShell::Welcome {
            incarnation: Incarnation(4),
            issue_epoch: 91,
        },
        ControlToShell::Settings {
            answer: false,
            view: view.clone(),
        },
        ControlToShell::Account(AccountView::SignedOut {
            revocation_unconfirmed: false,
        }),
        ControlToShell::Scenes { scenes: Vec::new() },
    ] {
        frame::write(&mut &control, &message, Pipe::Control).expect("written");
    }
    let issue_session = welcome(&shell_control).expect("the handshake takes Welcome");
    assert_eq!(
        issue_session,
        IssueSession {
            incarnation: Incarnation(4),
            epoch: 91
        }
    );
    let (sender, events) = async_channel::unbounded();
    let link = bare_link();
    let (_handle, start) = start(link, sender);
    let connection = Connection {
        control: Arc::new(shell_control),
        data: Arc::new(shell_data),
        issue_origin: issue_session,
    };
    let link = thread::spawn(move || start.link.attend(connection, &start.events));
    let events = patient(events);
    let session = connected(next(&events));
    assert_eq!(session.generation(), 1);
    assert_eq!(session.incarnation(), Incarnation(4));
    assert_eq!(
        session.issue_session(),
        IssueSession {
            incarnation: Incarnation(4),
            epoch: 91
        }
    );
    let LinkEvent::Settings {
        view: shown,
        answer: false,
        ..
    } = next(&events)
    else {
        panic!("settings first");
    };
    assert_eq!(shown, view);
    let LinkEvent::Account { generation, view } = next(&events) else {
        panic!("account follows settings");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(
        view,
        AccountView::SignedOut {
            revocation_unconfirmed: false
        }
    );
    let LinkEvent::Scenes(scenes) = next(&events) else {
        panic!("scenes after the account view");
    };
    assert!(scenes.is_empty());
    let error = kumochi_protocol::message::UserError::from(
        kumochi_protocol::message::ErrorKey::SignInTimedOut,
    );
    frame::write(
        &mut &control,
        &ControlToShell::Error(error.clone()),
        Pipe::Control,
    )
    .expect("error written");
    let LinkEvent::Error {
        generation,
        error: shown,
    } = next(&events)
    else {
        panic!("account error is delivered");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(shown, error);
    frame::write(&mut &control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    let pong = frame::read::<ControlToBrain>(&mut &control, Pipe::Control);
    assert_eq!(pong.expect("still connected"), Some(ControlToBrain::Pong));
    drop(control);
    assert!(link.join().expect("joined"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one two-connection scenario checks send fences, original frame provenance and credit isolation"
)]
fn an_ended_connection_s_sends_and_credit_never_reach_the_next() {
    use kumochi_protocol::message::{ApprovalRev, ContentHash, Decision, DraftId, IssueOp};
    let (a_control, a_shell_control) = pair("generation-a-control");
    let (a_data, a_shell_data) = pair("generation-a-data");
    let (b_control, b_shell_control) = pair("generation-b-control");
    let (b_data, b_shell_data) = pair("generation-b-data");
    for pipe in [&a_control, &a_data, &b_control, &b_data] {
        pipe.set_timeout(Some(PATIENCE));
    }
    let (sender, events) = async_channel::unbounded();
    let link = bare_link();
    let (handle, start) = start(link, sender);
    let link = thread::spawn(move || {
        [
            (a_shell_control, a_shell_data, 31),
            (b_shell_control, b_shell_data, 32),
        ]
        .into_iter()
        .all(|(control, data, epoch)| {
            let connection = Connection {
                control: Arc::new(control),
                data: Arc::new(data),
                issue_origin: IssueSession {
                    incarnation: Incarnation(1),
                    epoch,
                },
            };
            start.link.attend(connection, &start.events)
        })
    });
    let events = patient(events);
    let a = connected(next(&events));
    assert_eq!(
        a.issue_session(),
        IssueSession {
            incarnation: Incarnation(1),
            epoch: 31
        }
    );
    frame::write(&mut &a_data, &text(1, "from a"), Pipe::Data).expect("written");
    let LinkEvent::Data {
        generation, bytes, ..
    } = next(&events)
    else {
        panic!("a's data");
    };
    assert_eq!(generation, a.generation());
    // Ends A; the link then serves B on the same brain.
    drop(a_control);
    let b = connected(next(&events));
    assert_ne!(b.generation(), a.generation());
    assert_eq!(
        b.issue_session(),
        IssueSession {
            incarnation: Incarnation(1),
            epoch: 32
        }
    );
    let original = ControlToBrain::Issue {
        session: a.issue_session(),
        request: 0,
        op: Box::new(IssueOp::Consent {
            draft: DraftId(9),
            hash: ContentHash([7; 32]),
            approval: ApprovalRev(1),
            decision: Decision::File,
        }),
    };
    assert!(!handle.send_control(&a, original.clone()));
    assert!(handle.send_control(&b, original.clone()));
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &b_control, Pipe::Control).expect("original frame"),
        Some(original),
        "requeued frames retain their original issue provenance"
    );
    let fresh = ControlToBrain::Issue {
        session: b.issue_session(),
        request: 0,
        op: Box::new(IssueOp::Discard { draft: DraftId(9) }),
    };
    assert!(handle.send_control(&b, fresh.clone()));
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &b_control, Pipe::Control).expect("fresh frame"),
        Some(fresh)
    );
    // A UI that took A's frame only now, with B already installed.
    a.grant(bytes);
    let mut runs = Runs::default();
    runs.connected(a.generation());
    runs.settings(ThreadId(1));
    assert_eq!(
        runs.submit("typed", |turn| handle.send_data(&a, turn)),
        Err(Refused::Offline),
        "a turn through A's session never goes to B, so its draft stays"
    );
    assert!(!handle.send_control(&a, ControlToBrain::Pong));
    runs.disconnected();
    runs.connected(b.generation());
    runs.settings(ThreadId(1));
    assert_eq!(
        runs.submit("typed", |turn| handle.send_data(&b, turn)),
        Ok(())
    );
    b.grant(7);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &b_data, Pipe::Data).expect("read"),
        Some(DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "typed".into(),
        })
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &b_data, Pipe::Data).expect("read"),
        Some(DataToBrain::Credit { bytes: 7 }),
        "B gets its own credit and none of A's"
    );
    drop(b_control);
    assert!(link.join().expect("joined"));
}

/// Applies a link event to `runs` as the presence does, settling a delete's answer with the
/// link too; the credit it earns.
fn apply(runs: &mut Runs, handle: &LinkHandle, event: LinkEvent) -> u32 {
    match event {
        LinkEvent::RunState {
            run,
            thread,
            phase,
            origin,
            ..
        } => runs.apply_state(run, thread, phase, origin).credit,
        LinkEvent::Data { message, bytes, .. } => runs.apply_data(message, bytes),
        LinkEvent::ThreadDeleted {
            thread,
            outcome,
            last_run,
        } => {
            let credit = runs.deleted(thread, outcome, last_run);
            handle.settle_delete(thread);
            credit
        }
        LinkEvent::TurnRefused { turn, thread, .. } => {
            runs.refused(turn, thread);
            0
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn frame_size(message: &DataToShell) -> u32 {
    let size = frame::encode(message, Pipe::Data).expect("encodes").len();
    u32::try_from(size).expect("small")
}

fn ready_runs(session: &Session) -> Runs {
    let mut runs = Runs::default();
    runs.met(session.incarnation());
    runs.connected(session.generation());
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    runs
}

fn delete_of(thread: ThreadId, after_turns: u32) -> ControlToBrain {
    ControlToBrain::DeleteThread {
        thread,
        after_turns,
        resend: false,
    }
}

fn submit_message(turn: u32, thread: ThreadId, text: &str) -> DataToBrain {
    DataToBrain::TextSubmit {
        turn,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread: Some(thread),
        text: text.into(),
    }
}

#[test]
fn connections_reach_the_shell_with_their_session() {
    let link = attended("claude-connections");
    let events = patient(link.events);
    let session = connected(next(&events));
    let connections = kumochi_protocol::message::ConnectionsView {
        anthropic: kumochi_protocol::message::KeyView::Missing,
        anthropic_revision: None,
        claude_code: kumochi_protocol::message::ClaudeCodeView::Finding,
    };
    frame::write(
        &mut &link.control,
        &ControlToShell::Connections(connections),
        Pipe::Control,
    )
    .expect("connections written");
    let LinkEvent::Connections { generation, view } = next(&events) else {
        panic!("connections delivered");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(view, connections);

    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn a_delete_counts_the_turns_queued_before_it_and_runs_settle_the_brain_s_answers() {
    let (t, u, v) = (ThreadId(9), ThreadId(3), ThreadId(5));
    let link = attended("delete-history");
    let events = patient(link.events);
    let session = connected(next(&events));
    let handle = &link.handle;
    let send = |turn| handle.send_data(&session, turn);
    let mut runs = ready_runs(&session);
    assert!(runs.select_thread(Some(u)));
    runs.submit("u", send).expect("queued");
    assert!(runs.select_thread(Some(t)));
    runs.submit("t", send).expect("queued");
    assert!(runs.confirm_delete(t));
    handle.delete(t);
    assert!(handle.send_deletes(&session));
    for (turn, thread, typed) in [(1, u, "u"), (2, t, "t")] {
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
            Some(submit_message(turn, thread, typed))
        );
    }
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(delete_of(t, 2)),
        "the delete counts both turns queued before it"
    );

    // The brain read the delete before T's turn: U's run streams, T is hidden, and the fence
    // refuses T's turn. Each frame is taken before the next is written.
    frame::write(&mut &*link.data, &text(1, "a"), Pipe::Data).expect("written");
    let held = next(&events);
    let LinkEvent::Data { bytes, .. } = &held else {
        panic!("U's text, got {held:?}");
    };
    let bytes = *bytes;
    assert_eq!(
        apply(&mut runs, handle, held),
        0,
        "held for its run's state"
    );
    let answers = [
        run_state(1, u, Phase::Thinking, Some(RunOrigin::Typed { turn: 1 })),
        ControlToShell::ThreadDeleted {
            thread: t,
            outcome: Deleted::Hidden,
            last_run: None,
        },
        ControlToShell::TurnRefused {
            turn: 2,
            thread: Some(t),
        },
        run_state(1, u, Phase::Done, None),
    ];
    let mut credit = Vec::new();
    for answer in answers {
        frame::write(&mut &link.control, &answer, Pipe::Control).expect("written");
        credit.push(apply(&mut runs, handle, next(&events)));
        if matches!(answer, ControlToShell::ThreadDeleted { .. }) {
            assert!(
                runs.holds_for_tests(t),
                "T waits on the turn sent before its delete"
            );
            assert!(!runs.select_thread(Some(t)));
        }
    }
    assert_eq!(credit, [bytes, 0, 0, 0], "the held text's credit, once");
    assert!(!runs.holds_for_tests(t));
    assert!(runs.working(), "U's data hasn't ended");
    assert_eq!(
        runs.shown(),
        Some(&Shown {
            run: RunId(1),
            thread: Some(u),
            text: "a".to_owned(),
            cut: false,
            done: true,
        })
    );
    session.grant(bytes);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(DataToBrain::Credit { bytes })
    );
    // With U ended, only a turn still counted as pending would keep the runs working.
    let end = DataToShell::RunEnd { run: RunId(1) };
    frame::write(&mut &*link.data, &end, Pipe::Data).expect("written");
    assert_eq!(apply(&mut runs, handle, next(&events)), frame_size(&end));
    assert!(!runs.working(), "U's turn was answered and T's refused");

    runs.submit("next", send).expect("queued");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(submit_message(3, ThreadId(1), "next")),
        "confirming T's delete returned input to Kumo"
    );
    assert!(runs.confirm_delete(v));
    handle.delete(v);
    assert!(handle.send_deletes(&session));
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(delete_of(v, 3)),
        "T's answered delete isn't sent again"
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn a_delete_answered_before_a_new_brain_never_fences_the_new_brain_s_runs() {
    let (t, u) = (ThreadId(9), ThreadId(3));
    let (a_control, a_shell_control) = pair("reset-a-control");
    let (a_data, a_shell_data) = pair("reset-a-data");
    let (b_control, b_shell_control) = pair("reset-b-control");
    let (b_data, b_shell_data) = pair("reset-b-data");
    for pipe in [&a_control, &a_data, &b_control, &b_data] {
        pipe.set_timeout(Some(PATIENCE));
    }
    let (sender, events) = async_channel::unbounded();
    let link = bare_link();
    let (handle, start) = start(link, sender);
    let link = thread::spawn(move || {
        [
            (a_shell_control, a_shell_data, Incarnation(1)),
            (b_shell_control, b_shell_data, Incarnation(2)),
        ]
        .into_iter()
        .all(|(control, data, incarnation)| {
            let connection = Connection {
                control: Arc::new(control),
                data: Arc::new(data),
                issue_origin: IssueSession {
                    incarnation,
                    epoch: 1,
                },
            };
            start.link.attend(connection, &start.events)
        })
    });
    let events = patient(events);
    let a = connected(next(&events));
    let mut runs = ready_runs(&a);
    assert!(runs.confirm_delete(t));
    handle.delete(t);
    assert!(handle.send_deletes(&a));
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &a_control, Pipe::Control).expect("read"),
        Some(delete_of(t, 0))
    );
    let answer = ControlToShell::ThreadDeleted {
        thread: t,
        outcome: Deleted::Removed,
        last_run: Some(RunId(4)),
    };
    frame::write(&mut &a_control, &answer, Pipe::Control).expect("written");
    // A ends right after answering, and a restarted brain is reached next.
    drop(a_control);
    // Applying B's Connected here would panic: A's answer comes first.
    assert_eq!(apply(&mut runs, &handle, next(&events)), 0);
    assert!(!runs.holds_for_tests(t));
    let b = connected(next(&events));
    assert_eq!(b.incarnation(), Incarnation(2));
    runs.disconnected();
    runs.met(b.incarnation());
    runs.connected(b.generation());
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    assert!(handle.send_deletes(&b));
    assert!(handle.send_control(&b, ControlToBrain::Pong));
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &b_control, Pipe::Control).expect("read"),
        Some(ControlToBrain::Pong),
        "the answered delete isn't resent"
    );

    // The new brain numbers its runs afresh, so its run 4 isn't the deleted thread's.
    assert!(runs.select_thread(Some(u)));
    runs.submit("next", |turn| handle.send_data(&b, turn))
        .expect("queued");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &b_data, Pipe::Data).expect("read"),
        Some(DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(u),
            text: "next".into(),
        })
    );
    frame::write(
        &mut &b_control,
        &run_state(4, u, Phase::Answering, Some(RunOrigin::Typed { turn: 1 })),
        Pipe::Control,
    )
    .expect("written");
    assert_eq!(apply(&mut runs, &handle, next(&events)), 0);
    frame::write(&mut &b_data, &text(4, "b"), Pipe::Data).expect("written");
    let credit = apply(&mut runs, &handle, next(&events));
    assert_eq!(credit, frame_size(&text(4, "b")), "not fenced");
    assert_eq!(
        runs.shown(),
        Some(&Shown {
            run: RunId(4),
            thread: Some(u),
            text: "b".to_owned(),
            cut: false,
            done: false,
        })
    );
    drop(b_control);
    assert!(link.join().expect("joined"));
}

fn run_state(
    run: u64,
    thread: ThreadId,
    phase: Phase,
    origin: Option<RunOrigin>,
) -> ControlToShell {
    ControlToShell::RunState {
        origin,
        run: RunId(run),
        thread,
        phase,
        error: None,
    }
}

/// Plays the brain's control side: answers each Stop with `Interrupted`.
fn answer_stops(server: &PipeClient) {
    while let Ok(Some(message)) = frame::read::<ControlToBrain>(&mut &*server, Pipe::Control) {
        if let ControlToBrain::Stop { run, .. } = message {
            let state = ControlToShell::RunState {
                origin: None,
                run,
                thread: ThreadId(1),
                phase: Phase::Interrupted,
                error: None,
            };
            if frame::write(&mut &*server, &state, Pipe::Control).is_err() {
                return;
            }
        }
    }
}

/// Plays the brain's data side: streams [`TOTAL`] bytes in `chunk`-byte text frames, one
/// allocation per frame as the brain does, while another thread drains what the shell sends.
fn stream_text(server: &Arc<PipeClient>, chunk: usize) {
    let drain = Arc::clone(server);
    let drainer = thread::spawn(move || {
        let mut sink = [0; 4096];
        while std::io::Read::read(&mut &*drain, &mut sink).is_ok_and(|n| n > 0) {}
    });
    let mut sent = 0;
    while sent < TOTAL {
        let delta = "x".repeat(chunk.min(TOTAL - sent));
        sent += delta.len();
        let text = DataToShell::AssistantText {
            run: RunId(1),
            delta: delta.into(),
        };
        if frame::write(&mut &**server, &text, Pipe::Data).is_err() {
            break;
        }
    }
    // The shell closes its end once the connection ends, which ends the drain.
    drainer.join().expect("the drain ended cleanly");
}

#[expect(clippy::print_stderr, reason = "a measurement's output")]
fn report(name: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    eprintln!(
        "{name}: p50 {:?} p95 {:?} max {:?} (n={})",
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
        samples.len()
    );
}

/// Rez measurement for the pull request, not a check: run it with
/// `cargo test --release -p kumochi-shell -- --ignored --nocapture measure_`. The shell side
/// is the link as the UI uses it: a Stop queued for the control writer and its answer taken
/// from the event channel, and text taken from the event channel, granting credit for each
/// frame.
#[test]
#[ignore = "a measurement, run by hand on Rez"]
fn measure_control_round_trip_and_data_throughput() {
    let link = attended("measure-control");
    let control = Arc::new(link.control);
    let brain = thread::spawn({
        let control = Arc::clone(&control);
        move || answer_stops(&control)
    });
    let session = connected(link.events.recv_blocking().expect("an event"));
    let mut stops = Vec::new();
    for round in 0..200 {
        let run = RunId(round);
        let sent = Instant::now();
        assert!(link.handle.send_control(
            &session,
            ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            }
        ));
        while !matches!(
            link.events.recv_blocking(),
            Ok(LinkEvent::RunState { run: got, .. }) if got == run
        ) {}
        stops.push(sent.elapsed());
    }
    control.abort();
    brain.join().expect("the brain's side ended cleanly");
    // Closing the brain's end ends the connection.
    drop(control);
    assert!(link.link.join().expect("joined"));
    report("Stop -> Interrupted round trip", &mut stops);

    for chunk in [240_000, 40] {
        let mut streams = Vec::new();
        for round in 0..20 {
            let link = attended(&format!("measure-data-{chunk}-{round}"));
            link.data.set_timeout(None);
            let brain = thread::spawn({
                let data = Arc::clone(&link.data);
                move || stream_text(&data, chunk)
            });
            let session = connected(link.events.recv_blocking().expect("an event"));
            let mut started = None;
            let mut bytes = 0;
            while bytes < TOTAL {
                let Ok(LinkEvent::Data {
                    message: DataToShell::AssistantText { delta, .. },
                    bytes: size,
                    ..
                }) = link.events.recv_blocking()
                else {
                    panic!("text only");
                };
                started.get_or_insert_with(Instant::now);
                bytes += delta.len();
                session.grant(size);
            }
            streams.push(started.expect("text arrived").elapsed());
            drop(link.control);
            assert!(link.link.join().expect("joined"));
            brain.join().expect("the brain's side ended cleanly");
        }
        report(
            &format!("960 kB in {chunk}-byte frames, first frame to last"),
            &mut streams,
        );
    }
}

#[test]
fn both_drawing_controls_reach_the_ui_as_notices_with_their_own_keys() {
    let link = attended("drawing-notices");
    let events = patient(link.events);
    let session = connected(next(&events));
    for (control, expected) in [
        (
            ControlToShell::DrawingReplacementFailed {
                thread: ThreadId(3),
                run: RunId(2),
            },
            ErrorKey::DrawingReplacementFailed,
        ),
        (
            ControlToShell::DrawingNotReturned {
                thread: ThreadId(3),
                run: RunId(2),
            },
            ErrorKey::DrawingNotReturned,
        ),
    ] {
        frame::write(&mut &link.control, &control, Pipe::Control).expect("drawing notice");
        assert!(matches!(
            next(&events),
            LinkEvent::DrawingNotice {
                generation,
                key,
                thread: ThreadId(3),
                run: RunId(2),
            } if generation == session.generation() && key == expected
        ));
    }
}

#[path = "link_hold_tests.rs"]
mod hold_tests;

#[test]
fn a_prepared_link_has_no_speaker_opener() {
    let (sender, _events) = async_channel::unbounded();
    let (_handle, start) = super::prepare(sender).expect("prepares");
    assert!(
        start.link.speaker.is_none(),
        "a normal launch never opens an output"
    );
}

#[test]
fn a_normal_session_never_announces_an_output() {
    let link = attended("no-speaker");
    let events = patient(link.events);
    let _session = connected(next(&events));
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    loop {
        let message = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control)
            .expect("read")
            .expect("a frame");
        assert!(
            !matches!(message, ControlToBrain::AudioOutput { .. }),
            "no opener, no output"
        );
        if message == ControlToBrain::Pong {
            break;
        }
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn a_speaking_session_announces_its_output_keeps_speech_from_the_ui_and_answers_a_revoke() {
    use kumochi_protocol::message::{OutputGeneration, SegmentId, SpeechPcm};
    let opener = crate::speaker::Opener(Arc::new(|outlet| {
        Some(crate::speaker::deviceless(outlet, 24_000))
    }));
    let link = attended_speaking("speaker", opener);
    let events = patient(link.events);
    let _session = connected(next(&events));
    let read = || frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read");
    assert_eq!(
        read(),
        Some(ControlToBrain::AudioOutput {
            rate: 24_000,
            generation: OutputGeneration(1)
        })
    );
    let credit = |frame: &DataToShell| {
        let size = frame::encode(frame, Pipe::Data).expect("encodes").len();
        Some(DataToBrain::Credit {
            bytes: u32::try_from(size).expect("small"),
        })
    };
    let queued = DataToShell::SegmentQueued {
        run: RunId(1),
        segment: SegmentId(0),
        step: None,
    };
    frame::write(&mut &*link.data, &queued, Pipe::Data).expect("written");
    link.data.set_timeout(Some(Duration::from_millis(150)));
    assert!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).is_err(),
        "held, uncredited, until control names its run"
    );
    link.data.set_timeout(Some(PATIENCE));
    let state = ControlToShell::RunState {
        origin: None,
        run: RunId(1),
        thread: ThreadId(3),
        phase: Phase::Answering,
        error: None,
    };
    frame::write(&mut &link.control, &state, Pipe::Control).expect("state");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        credit(&queued),
        "credited once the speaker kept it"
    );
    let pcm = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(1),
        segment: SegmentId(0),
        output_generation: OutputGeneration(1),
        seq: 0,
        samples: vec![7; 240],
        last: Some(240),
    });
    frame::write(&mut &*link.data, &pcm, Pipe::Data).expect("written");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        credit(&pcm),
        "the audio went to the speaker and was credited once"
    );
    let revoke = ControlToShell::SpeechRevoked { run: RunId(1) };
    frame::write(&mut &link.control, &revoke, Pipe::Control).expect("revoke");
    assert_eq!(
        read(),
        Some(ControlToBrain::SpeechStopped {
            run: RunId(1),
            cursor: None
        })
    );
    assert!(
        matches!(next(&events), LinkEvent::RunState { run: RunId(1), .. }),
        "the UI still gets the run's state"
    );
    assert!(
        events.recv_timeout(Duration::from_millis(100)).is_err(),
        "speech never reaches the UI"
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

/// Opens no output however its worker ends, so the session must go on answering pings.
fn stays_up_without_output(label: &str, opener: crate::speaker::Opener) {
    let link = attended_speaking(label, opener);
    let events = patient(link.events);
    let _session = connected(next(&events));
    std::thread::sleep(Duration::from_millis(200));
    for _ in 0..2 {
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
            Some(ControlToBrain::Pong),
            "no output, and no lost connection"
        );
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"), "the UI was still there");
}

#[test]
fn a_speaker_that_fails_to_open_leaves_a_session_without_output() {
    stays_up_without_output(
        "refused-speaker",
        crate::speaker::Opener(Arc::new(|outlet| {
            let (speaker, worker) = crate::speaker::pair(outlet);
            drop(worker);
            drop(speaker);
            None
        })),
    );
    stays_up_without_output(
        "unaccepted-speaker",
        crate::speaker::Opener(Arc::new(crate::speaker::late)),
    );
    stays_up_without_output(
        "late-speaker",
        crate::speaker::Opener(Arc::new(|outlet| {
            let (_speaker, worker) = crate::speaker::pair(outlet);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                drop(worker);
            });
            None
        })),
    );
}

/// Waits for the link to return on its own, with the brain's ends still open.
fn returns_in_time(link: thread::JoinHandle<bool>, within: Duration) -> bool {
    let (done, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(link.join().expect("joined")));
    returned.recv_timeout(within).is_ok()
}

#[test]
fn an_opened_speaker_whose_worker_dies_ends_the_connection() {
    let opener = crate::speaker::Opener(Arc::new(|outlet| {
        let (mut speaker, worker) = crate::speaker::pair(outlet);
        speaker.attach(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(worker);
        }));
        Some(speaker)
    }));
    let link = attended_speaking("dying-speaker", opener);
    let events = patient(link.events);
    let _session = connected(next(&events));
    assert!(
        returns_in_time(link.link, PATIENCE),
        "no speech needed to notice"
    );
    drop(link.control);
}

#[test]
fn a_speaker_thread_left_behind_cannot_hold_up_teardown() {
    let (release, stuck) = std::sync::mpsc::channel::<()>();
    let stuck = std::sync::Mutex::new(Some(stuck));
    let opener = crate::speaker::Opener(Arc::new(move |outlet| {
        let (mut speaker, worker) = crate::speaker::pair(outlet);
        let stuck = stuck
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("opened once");
        speaker.attach(std::thread::spawn(move || {
            let _held = worker;
            let _released = stuck.recv();
        }));
        Some(speaker)
    }));
    let link = attended_speaking("stuck-speaker", opener);
    let events = patient(link.events);
    let _session = connected(next(&events));
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(ControlToBrain::Pong),
        "the writer's queue is empty again"
    );
    drop(link.control);
    assert!(
        returns_in_time(link.link, Duration::from_secs(5)),
        "the worker still holds a control sender"
    );
    release.send(()).expect("the worker was still held");
}

#[test]
fn audio_without_an_output_is_credited_and_never_shown() {
    use kumochi_protocol::message::{OutputGeneration, SegmentId, SpeechPcm};
    let link = attended("no-output-audio");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let pcm = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(1),
        segment: SegmentId(0),
        output_generation: OutputGeneration(1),
        seq: 0,
        samples: vec![7; 240],
        last: Some(240),
    });
    frame::write(&mut &*link.data, &pcm, Pipe::Data).expect("written");
    let size = frame::encode(&pcm, Pipe::Data).expect("encodes").len();
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
        Some(DataToBrain::Credit {
            bytes: u32::try_from(size).expect("small")
        })
    );
    assert!(
        events.recv_timeout(Duration::from_millis(100)).is_err(),
        "the samples never reach the UI"
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

/// A link whose speaker plays on a scripted device, and opens only when `go` sends; the last
/// receiver hears once the opener was entered. Named, playable speech for run 1 already waits
/// on its pipes.
fn opening_speaker(
    label: &str,
) -> (
    Attended,
    crate::speaker::fake::Shared,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
) {
    use kumochi_protocol::message::{OutputGeneration, SegmentId, SpeechPcm};
    let script = crate::speaker::fake::Shared::default();
    script.lock().expect("unpoisoned").play_to = u64::MAX;
    let (go, opening) = std::sync::mpsc::channel::<()>();
    let (enter, entered) = std::sync::mpsc::channel::<()>();
    let opening = std::sync::Mutex::new((enter, opening));
    let opener = crate::speaker::Opener(Arc::new({
        let script = Arc::clone(&script);
        move |outlet| {
            let opening = opening.lock().expect("unpoisoned");
            let _entered = opening.0.send(());
            let _opened = opening.1.recv();
            crate::speaker::fake::open(outlet, Arc::clone(&script))
        }
    }));
    let link = attended_speaking(label, opener);
    let state = ControlToShell::RunState {
        origin: None,
        run: RunId(1),
        thread: ThreadId(3),
        phase: Phase::Answering,
        error: None,
    };
    frame::write(&mut &link.control, &state, Pipe::Control).expect("state");
    let queued = DataToShell::SegmentQueued {
        run: RunId(1),
        segment: SegmentId(0),
        step: None,
    };
    let pcm = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(1),
        segment: SegmentId(0),
        output_generation: OutputGeneration(1),
        seq: 0,
        samples: vec![7; 2_400],
        last: Some(2_400),
    });
    for frame in [queued, pcm] {
        frame::write(&mut &*link.data, &frame, Pipe::Data).expect("written");
    }
    (link, script, go, entered)
}

fn played(script: &crate::speaker::fake::Shared) -> usize {
    script.lock().expect("unpoisoned").content.len()
}

/// Waits until the speaker took the queued speech: its credit reached the brain. The speech frame
/// alone carries at least a byte per sample, and the segment's announcement far less.
fn taken(data: &PipeClient) {
    let mut credited = 0;
    while credited < 2_400 {
        match frame::read::<DataToBrain>(&mut &*data, Pipe::Data).expect("read") {
            Some(DataToBrain::Credit { bytes }) => credited += bytes,
            Some(_) => {}
            None => panic!("the data pipe closed before the speech was credited"),
        }
    }
}

/// Waits for two more device passes, so anything playable would have been written.
fn two_passes(script: &crate::speaker::fake::Shared) {
    let passes = || script.lock().expect("unpoisoned").passes;
    let from = passes();
    let deadline = Instant::now() + PATIENCE;
    while passes() < from + 2 {
        assert!(Instant::now() < deadline, "two more passes");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn an_open_racing_the_inhibit_is_paused_before_any_speech_reaches_the_device() {
    use kumochi_protocol::message::GestureId;
    let (link, script, go, _entered) = opening_speaker("racing-inhibit");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let output = link.handle.output();
    let token = output.acquire(GestureId(1)).expect("taken while opening");
    go.send(()).expect("opening");
    taken(&link.data);
    two_passes(&script);
    assert_eq!(
        played(&script),
        0,
        "taken, and paused before any of it was written"
    );
    output
        .retire(token, super::output::Retirement::NoMic)
        .expect("retired");
    let deadline = Instant::now() + PATIENCE;
    while played(&script) < 2_400 {
        assert!(Instant::now() < deadline, "it was playable all along");
        thread::sleep(Duration::from_millis(5));
    }
    drop(link.control);
    assert!(link.link.join().expect("joined"), "the UI was still there");
}

#[test]
fn talk_back_off_while_opening_withdraws_the_late_open_before_any_speech() {
    let (link, script, go, _entered) = opening_speaker("off-while-opening");
    let events = patient(link.events);
    let _session = connected(next(&events));
    let output = link.handle.output();
    output.withdraw();
    go.send(()).expect("opening");
    let deadline = Instant::now() + PATIENCE;
    while output.view().why != Some(super::output::Why::Retired) {
        assert!(Instant::now() < deadline, "withdrawn and retired");
        thread::sleep(Duration::from_millis(5));
    }
    taken(&link.data);
    assert_eq!(played(&script), 0, "credited as stale, never written");
    drop(link.control);
    assert!(link.link.join().expect("joined"), "the UI was still there");
}

#[test]
fn an_open_that_returns_after_quit_began_announces_nothing() {
    a_late_open_announces_nothing(Loss::Quit, At::Opener);
}

#[test]
fn an_open_that_returns_after_the_session_was_lost_announces_nothing() {
    a_late_open_announces_nothing(Loss::Session, At::Opener);
}

#[test]
fn quit_after_the_link_checked_a_returned_open_still_refuses_it() {
    a_late_open_announces_nothing(Loss::Quit, At::Decision);
}

#[test]
fn a_session_lost_after_the_link_checked_a_returned_open_still_refuses_it() {
    a_late_open_announces_nothing(Loss::Session, At::Decision);
}

/// What ends a session while its speaker opens.
#[derive(Clone, Copy)]
enum Loss {
    Quit,
    Session,
}

/// When it ends: inside the blocked opener, or after the link's own check of the returned
/// speaker and before the owner's decision.
#[derive(Clone, Copy)]
enum At {
    Opener,
    Decision,
}

/// Quits or loses the session while its speaker opens; the speaker is never announced.
fn a_late_open_announces_nothing(loss: Loss, at: At) {
    use super::output::{OutputKey, OutputView, Why};
    let label = match (loss, at) {
        (Loss::Quit, At::Opener) => "quit-in-opener",
        (Loss::Quit, At::Decision) => "quit-before-decision",
        (Loss::Session, At::Opener) => "lost-in-opener",
        (Loss::Session, At::Decision) => "lost-before-decision",
    };
    let (link, script, go, entered) = opening_speaker(label);
    let events = patient(link.events);
    let session = connected(next(&events));
    let cancel = {
        let handle = link.handle.clone();
        move || match loss {
            Loss::Quit => assert!(handle.begin_quit(), "the first quit"),
            Loss::Session => session
                .alive()
                .store(false, std::sync::atomic::Ordering::Release),
        }
    };
    entered
        .recv_timeout(PATIENCE)
        .expect("the opener was entered");
    match at {
        At::Opener => cancel(),
        At::Decision => link.handle.output().before_opened(Box::new(cancel)),
    }
    go.send(()).expect("opening");
    let output = link.handle.output();
    let settled = OutputView {
        key: Some(OutputKey::NoOutput(1)),
        why: Some(Why::Refused),
        retiring: false,
        terminal: false,
    };
    let deadline = Instant::now() + PATIENCE;
    while output.view() != settled {
        assert!(Instant::now() < deadline, "{label}: let go, clean");
        thread::sleep(Duration::from_millis(5));
    }
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    loop {
        let message = frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control)
            .expect("read")
            .expect("a frame");
        assert!(
            !matches!(message, ControlToBrain::AudioOutput { .. }),
            "{label}: never announced"
        );
        if message == ControlToBrain::Pong {
            break;
        }
    }
    assert_eq!(played(&script), 0, "{label}: nothing reached the device");
    drop(link.control);
    let _ui = link.link.join().expect("joined");
}

#[test]
fn a_normal_session_has_no_output_and_still_lets_a_voice_hold_start() {
    use kumochi_protocol::message::GestureId;
    let link = attended("dormant-owner");
    let events = patient(link.events);
    let session = connected(next(&events));
    let output = link.handle.output();
    assert_eq!(
        output.view().why,
        Some(super::output::Why::NoOpener),
        "no opener, no output"
    );
    let token = output.acquire(GestureId(1)).expect("nothing to pause");
    let mic = link
        .handle
        .microphone_gate()
        .arm(GestureId(1))
        .expect("armed");
    output.bind_mic(&token, &mic).expect("bound");
    let claim = output
        .claim(&token, session.generation())
        .expect("claims the session's no-output key");
    assert!(output.start_mic(&claim).is_ok(), "the hold may start");
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(ControlToBrain::Pong),
        "only the pong: no announcement, no speaker command"
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"), "the UI was still there");
}
