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
use super::{Attendance, Connection, Link, LinkEvent, LinkHandle, Session, Shared, start, welcome};
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
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
            slug: kumochi_protocol::message::ModelSlug::new("gpt-5.5").expect("valid"),
            revision: kumochi_protocol::message::NoticeRevision::FIRST,
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
        queued_input: super::QueuedInput::default(),
        closing: Arc::default(),
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
    use kumochi_protocol::message::{
        OutputGeneration, SceneDescriptorV1, SceneHeader, SceneId, SceneSnapshotChunk, SegmentId,
        SpeechPromise,
    };
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
            speech: Some(SpeechPromise {
                output_generation: OutputGeneration(17),
                segment: SegmentId(4),
            }),
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
fn a_raw_segment_announcement_keeps_its_class_and_output_generation() {
    use kumochi_protocol::message::{OutputGeneration, SceneId, SegmentId, SegmentStart, StepRef};

    let link = attended("speech-metadata");
    let events = patient(link.events);
    let session = connected(next(&events));
    let sent = DataToShell::SegmentQueued {
        run: RunId(1),
        segment: SegmentId(3),
        step: Some(StepRef {
            scene: SceneId(7),
            index: 2,
        }),
        start: SegmentStart::Speech,
        output_generation: OutputGeneration(17),
    };
    frame::write(&mut &*link.data, &sent, Pipe::Data).expect("announcement written");
    let LinkEvent::Data {
        generation,
        message,
        bytes,
    } = next(&events)
    else {
        panic!("raw run-owned announcement");
    };
    assert_eq!(generation, session.generation());
    assert_eq!(message, sent);
    assert_eq!(
        usize::try_from(bytes).expect("size"),
        frame::encode(&sent, Pipe::Data).expect("frame").len()
    );
    session.grant(bytes);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("credit"),
        Some(DataToBrain::Credit { bytes }),
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

fn settings_view() -> kumochi_protocol::message::SettingsView {
    use kumochi_protocol::message::{AssistantName, SettingsView};
    SettingsView {
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
    }
}

#[test]
fn settings_frames_keep_their_connection_generation_and_authoritative_view() {
    let link = attended("settings-view");
    let events = patient(link.events);
    let session = connected(next(&events));
    let expected = settings_view();
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
fn a_closing_session_sends_no_held_delete_credit_or_owed_microphone_stop() {
    use super::microphone::Stop;
    use kumochi_protocol::message::GestureId;
    let link = attended("closing-session-sends");
    let events = patient(link.events);
    let session = connected(next(&events));
    let handle = &link.handle;
    *handle.shared.microphone_stop.lock().unwrap() = Some(Stop::Awaiting(GestureId(1)));
    session.close();
    handle.delete(ThreadId(9));
    assert!(!handle.send_deletes(&session), "no held delete");
    session.grant(7);
    assert_eq!(session.granted(), 0, "no credit");
    assert!(
        handle.send_control(
            &session,
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        ),
        "the stop is kept for the next connection"
    );
    assert!(
        matches!(
            *handle.shared.microphone_stop.lock().unwrap(),
            Some(Stop::Received {
                gesture: GestureId(1),
                sent_on: None
            })
        ),
        "but not sent on this one"
    );
    handle.end(&session);
    assert!(link.link.join().expect("the aborted session joins"));
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
        audio: super::AudioSession::Legacy,
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
    }
}

#[test]
fn real_handshake_writer_preserves_legacy_order_and_echoes_queued_welcome() {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    use kumochi_protocol::Secret;
    use kumochi_protocol::message::{AudioSession, CAPABILITY_LEN, Hello};

    for (mode, control_tag, data_tag) in [
        (AudioSession::Legacy, 0, 0),
        (AudioSession::QueuedRequired, 37, 9),
    ] {
        let (peer_control, control) = pair(&format!("hello-control-{control_tag}"));
        let (peer_data, data) = pair(&format!("hello-data-{data_tag}"));
        for pipe in [&peer_control, &control, &peer_data, &data] {
            pipe.set_timeout(Some(PATIENCE));
        }
        let allowed = Arc::new(AtomicBool::new(false));
        let connect = {
            let allowed = Arc::clone(&allowed);
            thread::spawn(move || {
                let hello = Hello {
                    build_id: "b".into(),
                    capability: Secret::new([7; CAPABILITY_LEN]),
                };
                let attempt =
                    (mode == AudioSession::QueuedRequired).then(|| Instant::now() + PATIENCE);
                super::handshake_pair(control, hello, mode, attempt, || {
                    assert!(allowed.load(Ordering::Acquire), "data opens after Welcome");
                    Ok(data)
                })
            })
        };
        let greeting = |tag, echo| {
            let mut payload = vec![tag, 1, b'b'];
            payload.extend([7; CAPABILITY_LEN]);
            if echo {
                payload.extend([5, 128, 1]);
            }
            let len = u32::try_from(payload.len()).unwrap().to_le_bytes();
            [len.as_slice(), &payload].concat()
        };
        let control_bytes = greeting(control_tag, false);
        let mut got = vec![0; control_bytes.len()];
        (&peer_control).read_exact(&mut got).unwrap();
        assert_eq!(got, control_bytes);
        allowed.store(true, Ordering::Release);
        frame::write(
            &mut &peer_control,
            &ControlToShell::Welcome {
                incarnation: Incarnation(5),
                issue_epoch: 128,
            },
            Pipe::Control,
        )
        .unwrap();
        let data_bytes = greeting(data_tag, mode == AudioSession::QueuedRequired);
        got.resize(data_bytes.len(), 0);
        (&peer_data).read_exact(&mut got).unwrap();
        assert_eq!(got, data_bytes);
        assert_eq!(
            connect.join().unwrap().unwrap().issue_origin,
            IssueSession {
                incarnation: Incarnation(5),
                epoch: 128
            }
        );
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
            speech: None,
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
        welcome(&mut &shell_control),
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
        welcome(&mut &shell_control),
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
    let issue_session = welcome(&mut &shell_control).expect("the handshake takes Welcome");
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
    let link =
        thread::spawn(move || start.link.attend(connection, &start.events) == Attendance::Lost);
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
            start.link.attend(connection, &start.events) == Attendance::Lost
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
            start.link.attend(connection, &start.events) == Attendance::Lost
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

#[test]
fn a_normal_session_has_no_output_and_still_lets_a_voice_hold_start() {
    use kumochi_protocol::message::GestureId;
    let link = attended("dormant-owner");
    let events = patient(link.events);
    let session = connected(next(&events));
    let output = link.handle.output();
    assert_eq!(
        output.view().expect("uncontended").why,
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
#[test]
fn queued_owner_refuses_turns_and_ends_both_pipes_on_withdrawal() {
    use crate::link::output::Denied;
    use kumochi_protocol::message::{GestureId, ScreenChoice};

    let link = attended_queued("queued-owner-withdrawal");
    let events = patient(link.events);
    let session = connected(next(&events));
    assert!(session.open());
    assert_eq!(link.handle.output().view().unwrap().key, None);
    assert!(matches!(
        link.handle.output().acquire(GestureId(3)),
        Err(Denied::Inhibited)
    ));
    assert!(!link.handle.send_data(
        &session,
        DataToBrain::TextSubmit {
            turn: 1,
            thread: None,
            screen: ScreenChoice::None,
            text: "early".into(),
        }
    ));
    link.handle.output().end(session.generation() + 1);
    assert!(session.open(), "a stale end cannot affect this pair");
    link.handle.output().withdraw();
    assert!(
        !session.open(),
        "withdrawal closes sends before waking the pipes"
    );
    queued_pair_closed(link.link, &link.control, &link.data);
}

#[test]
fn queued_owner_start_deadline_is_not_extended_by_control_traffic() {
    let start = Instant::now();
    let link = attended_queued("queued-owner-deadline");
    let events = patient(link.events);
    let session = connected(next(&events));
    let mut pongs = 0;
    while session.open() {
        assert!(
            start.elapsed() < PATIENCE * 2,
            "the startup deadline is absolute"
        );
        if frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).is_err() {
            break;
        }
        match frame::read(&mut &link.control, Pipe::Control).unwrap() {
            Some(ControlToBrain::Pong) => pongs += 1,
            None => break,
            Some(other) => panic!("unexpected control before registration: {other:?}"),
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(pongs > 0, "the control reader was active before expiry");
    queued_pair_closed(link.link, &link.control, &link.data);
    assert!(!session.open());
}

fn queued_registration(control: &PipeClient) -> kumochi_protocol::message::OutputGeneration {
    frame::write(
        &mut &*control,
        &ControlToShell::Settings {
            view: kumochi_protocol::message::SettingsView {
                talk_back: true,
                ..settings_view()
            },
            answer: false,
        },
        Pipe::Control,
    )
    .expect("talk-back settings written");
    match frame::read(&mut &*control, Pipe::Control).expect("read registration") {
        Some(ControlToBrain::QueuedAudioOutput(24_000, output)) => output,
        other => panic!("a 24 kHz queued registration, got {other:?}"),
    }
}

fn acknowledge_queued(control: &PipeClient, output: kumochi_protocol::message::OutputGeneration) {
    frame::write(
        &mut &*control,
        &ControlToShell::QueuedAudioRegistered(output),
        Pipe::Control,
    )
    .expect("registered");
    frame::write(&mut &*control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read(&mut &*control, Pipe::Control).expect("peer reads Pong"),
        Some(ControlToBrain::Pong),
        "the control reader has handled the prior ACK"
    );
}

fn queued_text(turn: u32, text: &str) -> DataToBrain {
    DataToBrain::TextSubmit {
        turn,
        thread: Some(ThreadId(1)),
        screen: kumochi_protocol::message::ScreenChoice::None,
        text: text.into(),
    }
}

#[test]
fn queued_talk_back_waits_for_both_ends_of_a_deleted_reply() {
    use super::output::Denied;
    use kumochi_protocol::message::{Deleted, SettingsOp};

    for (data_first, outcome) in [(true, Deleted::Hidden), (false, Deleted::Removed)] {
        let link = attended_queued("queued-talk-back-delete");
        let events = patient(link.events);
        let session = connected(next(&events));
        let control = |message: ControlToShell| {
            frame::write(&mut &link.control, &message, Pipe::Control).unwrap();
        };
        control(ControlToShell::Settings {
            view: settings_view(),
            answer: false,
        });
        let Some(ControlToBrain::QueuedAudioOutput(24_000, output)) =
            frame::read(&mut &link.control, Pipe::Control).unwrap()
        else {
            panic!("silent pair registers");
        };
        acknowledge_queued(&link.control, output);
        assert!(matches!(next(&events), LinkEvent::Settings { .. }));
        let text = queued_text(1, "before deletion");
        assert!(link.handle.send_data(&session, text.clone()));
        assert_eq!(
            frame::read(&mut &*link.data, Pipe::Data).unwrap(),
            Some(text)
        );
        control(run_state(
            7,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        ));
        assert!(matches!(
            next(&events),
            LinkEvent::RunState { run: RunId(7), .. }
        ));

        let data_end = || {
            frame::write(
                &mut &*link.data,
                &DataToShell::RunEnd { run: RunId(7) },
                Pipe::Data,
            )
            .unwrap();
            assert!(matches!(
                next(&events),
                LinkEvent::Data {
                    message: DataToShell::RunEnd { run: RunId(7) },
                    ..
                }
            ));
        };
        let delete = |thread, outcome, last_run| {
            control(ControlToShell::ThreadDeleted {
                thread,
                outcome,
                last_run: Some(last_run),
            });
            assert!(matches!(
                next(&events),
                LinkEvent::ThreadDeleted { thread: t, outcome: o, last_run: Some(r) }
                    if (t, o, r) == (thread, outcome, last_run)
            ));
        };
        let offer = || link.handle.send_talk_back(&session, true);
        if data_first {
            data_end();
            assert_eq!(offer(), Err(Denied::Busy));
            for (thread, outcome, run) in [
                (ThreadId(1), Deleted::NotHidden, RunId(7)),
                (ThreadId(1), Deleted::Hidden, RunId(8)),
                (ThreadId(2), Deleted::Removed, RunId(7)),
            ] {
                delete(thread, outcome, run);
                assert_eq!(offer(), Err(Denied::Busy));
            }
        }
        delete(ThreadId(1), outcome, RunId(7));
        if !data_first {
            assert_eq!(offer(), Err(Denied::Busy));
            data_end();
        }
        let after_both = offer();
        if after_both.is_ok() {
            assert_eq!(
                frame::read(&mut &link.control, Pipe::Control).unwrap(),
                Some(ControlToBrain::Settings(SettingsOp::SetTalkBack(true)))
            );
        }
        link.handle.end(&session);
        assert!(link.link.join().unwrap());
        assert_eq!(
            after_both,
            Ok(()),
            "matching deletion and RunEnd settle the idle fence"
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn queued_admission_waits_for_the_outbox_but_the_first_voice_edge_refuses() {
    use std::sync::mpsc::{self, RecvTimeoutError};

    let link = attended_queued("queued-admission-contended");
    let events = patient(link.events);
    let session = connected(next(&events));
    let generation = session.generation();
    frame::write(
        &mut &link.control,
        &ControlToShell::Settings {
            view: kumochi_protocol::message::SettingsView {
                show_screen: false,
                ..settings_view()
            },
            answer: false,
        },
        Pipe::Control,
    )
    .unwrap();
    let Some(ControlToBrain::QueuedAudioOutput(_, output)) =
        frame::read(&mut &link.control, Pipe::Control).unwrap()
    else {
        panic!("a queued output registration");
    };
    acknowledge_queued(&link.control, output);

    let (starting, started) = mpsc::channel();
    let (text_result, typed) = mpsc::channel();
    let (voice_result, voiced) = mpsc::channel();
    let mut while_held = None;
    let handle = &link.handle;
    let pair = &session;
    thread::scope(|scope| {
        with_outbox_held(handle, pair, || {
            let text_starting = starting.clone();
            scope.spawn(move || {
                text_starting.send(()).unwrap();
                text_result.send(handle.admits_text(Some(pair))).unwrap();
            });
            scope.spawn(move || {
                starting.send(()).unwrap();
                voice_result.send(handle.admits_voice(generation)).unwrap();
            });
            for _ in 0..2 {
                started.recv_timeout(PATIENCE).unwrap();
            }
            let native = handle.can_start_voice_gesture(generation);
            let window = Duration::from_millis(125);
            while_held = Some((
                native,
                typed.recv_timeout(window),
                voiced.recv_timeout(window),
            ));
        });
    });
    let after_release = (typed.recv_timeout(PATIENCE), voiced.recv_timeout(PATIENCE));

    let text = queued_text(1, "accepted after the outbox is released");
    assert!(handle.send_data(pair, text.clone()));
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).unwrap(),
        Some(text)
    );
    handle.end(pair);
    queued_pair_closed(link.link, &link.control, &link.data);
    assert_eq!(
        while_held,
        Some((
            false,
            Err(RecvTimeoutError::Timeout),
            Err(RecvTimeoutError::Timeout)
        )),
        "the native edge refuses immediately, while both authoritative checks wait"
    );
    assert_eq!(after_release, (Ok(true), Ok(true)));
}

#[test]
fn queued_hold_waits_for_its_saved_preferences_and_rejects_another_run_origin() {
    use kumochi_protocol::message::{GestureId, HoldStatus, OutputGeneration, SettingsOp};

    let link = attended_queued("queued-hold-consent");
    let events = patient(link.events);
    let session = connected(next(&events));
    let generation = session.generation();
    let gesture = GestureId(41);
    let recognized = ControlToBrain::VoiceHoldRecognized {
        gesture,
        thread: None,
        selection: 1,
    };
    let send = |msg| frame::write(&mut &link.control, &msg, Pipe::Control).unwrap();
    let receive = || frame::read(&mut &link.control, Pipe::Control).unwrap();
    let settings = |show_screen, answer| {
        send(ControlToShell::Settings {
            view: kumochi_protocol::message::SettingsView {
                show_screen,
                ..settings_view()
            },
            answer,
        });
        assert!(
            matches!(next(&events), LinkEvent::Settings { generation: actual, .. } if actual == generation)
        );
    };

    link.handle.track_input(gesture);
    assert!(
        !link.handle.admits_voice(generation),
        "settings are unknown"
    );
    assert!(!link.handle.send_control(&session, recognized.clone()));
    assert!(
        link.handle
            .send_control(&session, ControlToBrain::VoiceHoldStopped { gesture })
    );
    link.handle.clear_input(gesture);
    settings(true, false);
    let Some(ControlToBrain::QueuedAudioOutput(_, output)) = receive() else {
        panic!("the pair offers its logical output");
    };
    assert_ne!(output, OutputGeneration(0));
    acknowledge_queued(&link.control, output);
    assert!(link.handle.admits_voice(generation), "Show Screen is set");

    for op in [
        SettingsOp::SetTaskbar(false),
        SettingsOp::SetShowScreen(false),
    ] {
        let command = ControlToBrain::Settings(op);
        assert!(link.handle.send_control(&session, command.clone()));
        assert_eq!(receive(), Some(command));
    }
    link.handle.track_input(gesture);
    settings(false, false);
    settings(false, true);
    assert!(
        !link.handle.admits_voice(generation),
        "the earlier settings answer didn't settle Show Screen"
    );
    assert!(!link.handle.send_control(&session, recognized.clone()));
    assert!(
        !link.handle.voice_stop_pending(),
        "a refused offer owns no microphone debt"
    );
    settings(false, true);
    assert!(link.handle.admits_voice(generation));
    assert!(
        !link.handle.admits_voice(generation + 1),
        "a different pair cannot use those preferences"
    );
    link.handle.track_input(gesture);
    assert!(link.handle.send_control(&session, recognized.clone()));
    assert_eq!(receive(), Some(recognized));

    send(ControlToShell::VoiceHoldStatus {
        gesture,
        status: HoldStatus::Admitted,
    });
    assert!(
        matches!(next(&events), LinkEvent::VoiceHold { gesture: key, status: HoldStatus::Admitted, .. } if key == gesture)
    );
    let released = ControlToBrain::KeyUp {
        gesture,
        t_qpc: 5,
        chunks: 0,
    };
    assert!(link.handle.send_control(&session, released.clone()));
    assert_eq!(receive(), Some(released));
    send(run_state(
        1,
        ThreadId(1),
        Phase::Thinking,
        Some(RunOrigin::Hold {
            gesture: GestureId(42),
        }),
    ));
    queued_pair_closed(link.link, &link.control, &link.data);
    assert!(!session.open());
    assert!(
        link.handle.voice_stop_pending(),
        "closing the pair didn't settle the real microphone debt"
    );
}

#[test]
fn a_queued_screen_release_goes_only_while_its_hold_listens_unrevoked() {
    use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};
    use kumochi_protocol::message::{
        GestureId, HoldStatus, LeaseGeneration, NoticeRevision, SettingsOp, SettingsView,
    };

    // (the shell turns Show Screen off itself, the brain's answer changes it)
    for (sent, answered) in [(false, false), (true, false), (false, true)] {
        let link = attended_queued("queued-screen-release");
        let events = patient(link.events);
        let session = connected(next(&events));
        let generation = session.generation();
        let gesture = GestureId(41);
        let send = |msg| frame::write(&mut &link.control, &msg, Pipe::Control).unwrap();
        let receive = || frame::read(&mut &link.control, Pipe::Control).unwrap();
        let settings = |show_screen| {
            send(ControlToShell::Settings {
                view: SettingsView {
                    show_screen,
                    ..settings_view()
                },
                answer: false,
            });
            assert!(
                matches!(next(&events), LinkEvent::Settings { generation: actual, .. } if actual == generation)
            );
        };
        settings(true);
        let Some(ControlToBrain::QueuedAudioOutput(_, output)) = receive() else {
            panic!("the pair offers its logical output");
        };
        acknowledge_queued(&link.control, output);
        let recognized = ControlToBrain::ScreenHoldRecognized {
            gesture,
            thread: None,
            selection: 1,
            t_qpc: 1,
            notice: Some(NoticeRevision::FIRST),
        };
        let released = ControlToBrain::ScreenRelease {
            gesture,
            lease: LeaseGeneration(1),
            t_qpc: 2,
            excluded_qpc: 2,
            pointer: ReleasePointer(PhysicalPoint([5, 5])),
            path: PointerPath(Vec::new()),
            notice: Some(NoticeRevision::FIRST),
        };
        link.handle.track_input(gesture);
        link.handle.watch_revoke(gesture);
        assert!(link.handle.send_control(&session, recognized.clone()));
        assert_eq!(receive(), Some(recognized));
        assert!(
            !link.handle.send_control(&session, released.clone()),
            "the hold isn't listening yet"
        );
        send(ControlToShell::VoiceHoldStatus {
            gesture,
            status: HoldStatus::Admitted,
        });
        assert!(
            matches!(next(&events), LinkEvent::VoiceHold { gesture: key, status: HoldStatus::Admitted, .. } if key == gesture)
        );
        if sent {
            let off = ControlToBrain::Settings(SettingsOp::SetShowScreen(false));
            assert!(link.handle.send_control(&session, off.clone()));
            assert_eq!(receive(), Some(ControlToBrain::GestureCancel { gesture }));
            assert_eq!(receive(), Some(off));
        }
        if answered {
            settings(false);
        }
        let revoked = sent || answered;
        assert_eq!(
            link.handle.send_control(&session, released.clone()),
            !revoked,
            "sent {sent}, answered {answered}"
        );
        assert_eq!(link.handle.was_revoked(gesture), revoked);
        if !revoked {
            assert_eq!(receive(), Some(released));
            // An answered hold's input is cleared, so a later Show Screen change revokes nothing.
            link.handle.clear_input(gesture);
            settings(false);
            assert!(!link.handle.was_revoked(gesture));
        }
        send(run_state(
            1,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Hold {
                gesture: GestureId(42),
            }),
        ));
        queued_pair_closed(link.link, &link.control, &link.data);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one authenticated pair proves cancel, release and reply in order"
)]
fn authenticated_queued_hold_cancels_or_transfers_only_the_released_gesture() {
    use kumochi_protocol::message::{GestureId, HoldStatus, MicPcm};

    let _device = serialize_playback();
    let (id, hello, probe, brain) = kumochi_brain::ipc::offline_hold_peer();
    let pipe = authenticated_primed(id, hello);
    let events = patient(pipe.events);
    let session = connected(next(&events));
    let generation = session.generation();
    let LinkEvent::Settings { view, .. } = next(&events) else {
        panic!("the paired Brain first reports the saved settings");
    };
    assert!(!view.talk_back && !view.show_screen && view.hold_to_talk);
    let recognized = |gesture| ControlToBrain::VoiceHoldRecognized {
        gesture,
        thread: None,
        selection: 1,
    };
    let packet = |gesture| {
        DataToBrain::MicPcm(MicPcm {
            gesture,
            rate: 16_000,
            seq: 0,
            last: true,
            samples: vec![173, -19],
        })
    };
    let handle = &pipe.handle;
    let cancelled = GestureId(17);
    handle.track_input(cancelled);
    assert!(!handle.send_control(&session, recognized(cancelled)));
    assert!(!handle.send_data(&session, packet(cancelled)));
    assert!(!handle.voice_stop_pending());
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();

    let until = Instant::now() + PATIENCE;
    while !handle.admits_voice(generation) {
        assert!(Instant::now() < until && session.open());
        thread::sleep(Duration::from_millis(2));
    }
    let expect_status = |gesture, status| loop {
        match next(&events) {
            LinkEvent::VoiceHold {
                generation: from,
                gesture: key,
                status: seen,
            } => {
                assert_eq!((from, key, seen), (generation, gesture, status));
                break;
            }
            LinkEvent::RunState { .. } | LinkEvent::CaptureDone { .. } => {
                panic!("an unsubmitted Hold creates neither a run nor a capture");
            }
            _ => {}
        }
    };
    let stop = |gesture| {
        assert!(
            handle.voice_stop_pending(),
            "only an owed Stop may be offered"
        );
        assert!(handle.send_control(&session, ControlToBrain::VoiceHoldStopped { gesture }));
        let until = Instant::now() + PATIENCE;
        while handle.voice_stop_pending() {
            assert!(Instant::now() < until && session.open());
            thread::sleep(Duration::from_millis(2));
        }
    };

    assert!(handle.send_control(&session, recognized(cancelled)));
    expect_status(cancelled, HoldStatus::Admitted);
    let writer = &handle.shared.hold_pcm_writer;
    writer.store(1, std::sync::atomic::Ordering::Release);
    assert!(handle.send_data(&session, packet(cancelled)));
    let until = Instant::now() + PATIENCE;
    while writer.load(std::sync::atomic::Ordering::Acquire) != 2 {
        assert!(
            Instant::now() < until && session.open(),
            "the encoded PCM hasn't reached the pipe"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let cancelled_sent = handle.send_control(
        &session,
        ControlToBrain::GestureCancel { gesture: cancelled },
    );
    writer.store(0, std::sync::atomic::Ordering::Release);
    assert!(cancelled_sent);
    assert!(!handle.send_data(&session, packet(cancelled)));
    expect_status(cancelled, HoldStatus::Cancelled);
    stop(cancelled);
    assert_eq!(probe.decoded(), 0);

    let gesture = GestureId(18);
    handle.track_input(gesture);
    assert!(handle.send_control(&session, recognized(gesture)));
    expect_status(gesture, HoldStatus::Admitted);
    assert!(!handle.send_data(&session, packet(cancelled)));
    handle
        .output()
        .with_locked(|| assert!(handle.send_data(&session, packet(gesture))));
    stop(gesture);
    assert!(
        !handle.admits_text(Some(&session)),
        "a mic receipt is not a run transfer"
    );
    let released = |gesture| ControlToBrain::KeyUp {
        gesture,
        t_qpc: 2,
        chunks: 1,
    };
    assert!(!handle.send_control(&session, released(cancelled)));
    handle
        .output()
        .with_locked(|| assert!(handle.send_control(&session, released(gesture))));
    assert!(!handle.send_data(&session, packet(gesture)));

    let mut answer = String::new();
    let (mut named, mut done, mut ended, mut held) = (false, false, false, false);
    let expected_run = RunId(1);
    let (mut sent_barriers, mut control_barrier, mut data_barrier) = (false, false, false);
    while !(control_barrier && data_barrier) {
        match next(&events) {
            LinkEvent::RunState {
                generation: from,
                run,
                phase: Phase::Thinking,
                origin,
                ..
            } => {
                assert!(!named);
                assert_eq!(
                    (from, run, origin),
                    (generation, expected_run, Some(RunOrigin::Hold { gesture }))
                );
                named = true;
            }
            LinkEvent::RunState {
                run,
                phase: Phase::Done,
                ..
            } => {
                assert!(!done, "one Done before the later control reply");
                assert_eq!(run, expected_run);
                done = true;
            }
            LinkEvent::Data { message, bytes, .. } => {
                match message {
                    DataToShell::AssistantText { run, delta } => {
                        assert_eq!(run, expected_run);
                        answer.push_str(delta.as_str());
                    }
                    DataToShell::HoldWords {
                        gesture: key,
                        words,
                    } => {
                        assert_eq!((key, words.as_str()), (gesture, "synthetic held words"));
                        held = true;
                    }
                    DataToShell::RunEnd { run } => {
                        assert!(!ended, "one RunEnd before the later data reply");
                        assert_eq!(run, expected_run);
                        ended = true;
                    }
                    DataToShell::ThreadList { request: 37, .. } => data_barrier = true,
                    DataToShell::SegmentQueued { .. } | DataToShell::SpeechPcm(_) => {
                        panic!("a silent Hold emits no speaker work");
                    }
                    _ => {}
                }
                session.grant(bytes);
            }
            LinkEvent::Settings { answer: true, .. } => control_barrier = true,
            LinkEvent::TurnRefused { .. }
            | LinkEvent::Error { .. }
            | LinkEvent::CaptureDone { .. } => {
                panic!("the ordinary Hold finishes without a refusal or a capture");
            }
            _ => {}
        }
        if named && done && ended && held && !sent_barriers {
            sent_barriers = true;
            assert!(handle.send_control(
                &session,
                ControlToBrain::Settings(super::SettingsOp::SetTaskbar(false))
            ));
            assert!(handle.send_data(
                &session,
                DataToBrain::PageRequest {
                    request: 37,
                    page: PageOf::Threads {
                        search: None,
                        after: None
                    },
                }
            ));
        }
    }
    assert_eq!(answer, "An offline voice reply.");
    assert_eq!(probe.decoded(), 1);
    assert_eq!(
        probe.received_mic(),
        1,
        "only the released gesture's PCM crossed the actual data pipe"
    );
    assert!(probe.received_retirements(expected_run).is_empty());
    assert!(handle.admits_text(Some(&session)));
    handle.end(&session);
    assert!(!handle.send_control(&session, recognized(GestureId(19))));
    assert!(!handle.send_data(&session, packet(gesture)));
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
    drop(pipe.handle);
    assert!(matches!(
        pipe.playback.recv_timeout(PATIENCE),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn authenticated_queued_text_replies_with_talk_back_off() {
    use kumochi_protocol::message::AudioSession;

    let _device = serialize_playback();
    let (id, hello, probe, brain) =
        kumochi_brain::ipc::offline_text_peer("0 text A written reply.".into());
    let pipe = authenticated_primed(id, hello);
    let events = patient(pipe.events);
    let session = connected(next(&events));
    let generation = session.generation();
    let settings = events
        .recv_timeout(PATIENCE)
        .expect("Talk-back off must not tear down the authenticated queued pair");
    let LinkEvent::Settings { view, .. } = settings else {
        panic!("the first application frame is Settings");
    };
    assert!(!view.talk_back);
    assert_eq!(pipe.handle.audio_session(), AudioSession::QueuedRequired);
    assert!(!pipe.handle.admits_text(Some(&session)));
    assert!(!pipe.handle.send_data(&session, queued_text(1, "early")));

    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let deadline = Instant::now() + PATIENCE;
    while !pipe.handle.admits_text(Some(&session)) {
        assert!(
            Instant::now() < deadline && session.open(),
            "matching registration"
        );
        thread::sleep(Duration::from_millis(2));
    }
    for turn in 1..=2 {
        let id = RunId(u64::from(turn));
        assert!(pipe.handle.send_data(&session, queued_text(turn, "write")));
        let mut answer = String::new();
        let (mut named, mut done, mut ended) = (false, false, false);
        while !(named && done && ended) {
            match next(&events) {
                LinkEvent::RunState {
                    generation: connected,
                    run,
                    phase: Phase::Thinking,
                    origin,
                    ..
                } => {
                    assert_eq!(
                        (connected, run, origin),
                        (generation, id, Some(RunOrigin::Typed { turn }))
                    );
                    named = true;
                }
                LinkEvent::RunState {
                    run,
                    phase: Phase::Done,
                    ..
                } => {
                    assert_eq!(run, id);
                    done = true;
                }
                LinkEvent::Data { message, bytes, .. } => {
                    match message {
                        DataToShell::AssistantText { run, delta } => {
                            assert_eq!(run, id);
                            answer.push_str(delta.as_str());
                        }
                        DataToShell::RunEnd { run } => {
                            assert_eq!(run, id);
                            ended = true;
                        }
                        _ => {}
                    }
                    session.grant(bytes);
                }
                LinkEvent::TurnRefused { .. } | LinkEvent::Error { .. } => {
                    panic!("the text turn must finish normally")
                }
                _ => {}
            }
        }
        assert_eq!(answer, "A written reply.");
        assert!(probe.received_retirements(id).is_empty());
    }
    assert!(session.open());
    pipe.handle.end(&session);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
    drop(pipe.handle);
    assert!(matches!(
        pipe.playback.recv_timeout(PATIENCE),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    assert_eq!(probe.heard(), 0);
    assert_eq!(probe.quiet_receipt(), None);
}

#[cfg(debug_assertions)]
#[test]
fn burst_playback_reports_the_entire_due_source_to_brain() {
    use kumochi_protocol::message::{Cursor, SegmentId};

    const TOTAL: u64 = 3_840;
    let _device = serialize_playback();
    let (id, hello, probe, brain) =
        kumochi_brain::ipc::offline_plain_peer("0 text An offline voice reply.".into(), 16);
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let pipe = authenticated_primed(id, hello);
    let events = patient(pipe.events);
    let session = connected(next(&events));
    let limit = Instant::now() + PATIENCE;
    while !pipe.handle.admits_text(Some(&session)) {
        assert!(
            Instant::now() < limit && session.open(),
            "queued audio registered"
        );
        thread::yield_now();
    }
    assert!(pipe.handle.send_data(&session, queued_text(1, "speak")));
    let mut playback = pipe.playback.recv_timeout(PATIENCE).unwrap();
    let run = RunId(1);
    let limit = Instant::now() + PATIENCE;
    let (mut done, mut ended) = (false, false);
    while !(done && ended && probe.quiet() == run) {
        assert!(
            Instant::now() < limit && session.open(),
            "the spoken run finishes naturally"
        );
        for event in events.try_iter() {
            match event {
                LinkEvent::RunState {
                    run: id,
                    phase: Phase::Done,
                    ..
                } => {
                    assert_eq!(id, run);
                    done = true;
                }
                LinkEvent::Data { message, bytes, .. } => {
                    if let DataToShell::RunEnd { run: id } = message {
                        assert_eq!(id, run);
                        ended = true;
                    }
                    session.grant(bytes);
                }
                LinkEvent::TurnRefused { .. } | LinkEvent::Error { .. } => {
                    panic!("the spoken turn must finish normally")
                }
                _ => {}
            }
        }
        for _ in 0..16 {
            playback.render(&mut [0.0; 48]);
        }
        thread::yield_now();
    }
    assert_eq!(playback.handed(), TOTAL);
    assert_eq!(playback.presented_prefix(), TOTAL);
    let sample = u32::try_from(TOTAL).unwrap();
    let cursor = Cursor {
        segment: SegmentId(0),
        sample,
    };
    let receipts = probe.retired(run);
    let [final_receipt] = receipts
        .iter()
        .filter(|receipt| receipt.final_segment)
        .collect::<Vec<_>>()[..]
    else {
        panic!("Brain accepts one final retirement, got {receipts:?}");
    };
    assert_eq!(
        (
            final_receipt.segment,
            final_receipt.retired,
            final_receipt.heard
        ),
        (cursor.segment, sample, sample)
    );
    assert_eq!(probe.quiet_receipt(), Some((run, Some(cursor))));
    assert_eq!(probe.stop(), RunId(0));
    pipe.handle.end(&session);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
}

#[derive(Clone, Copy)]
enum SilentSpeechAt {
    Live,
    Stopped,
    Ended,
    Future,
}

#[test]
fn queued_speech_on_a_talk_back_off_run_closes_without_credit() {
    assert_silent_speech_refused("queued-silent-live", SilentSpeechAt::Live);
}

#[test]
fn queued_speech_on_a_talk_back_off_run_after_stop_closes_without_credit() {
    assert_silent_speech_refused("queued-silent-stopped", SilentSpeechAt::Stopped);
}

#[test]
fn queued_speech_on_a_talk_back_off_run_after_end_closes_without_credit() {
    assert_silent_speech_refused("queued-silent-ended", SilentSpeechAt::Ended);
}

#[test]
fn queued_speech_on_a_talk_back_off_run_before_its_name_closes_without_waiting() {
    assert_silent_speech_refused("queued-silent-future", SilentSpeechAt::Future);
}

#[expect(
    clippy::too_many_lines,
    reason = "one attended pair exercises each boundary from registration to teardown"
)]
fn assert_silent_speech_refused(label: &str, at: SilentSpeechAt) {
    use kumochi_protocol::message::{SegmentId, SegmentStart};

    let _device = serialize_playback();
    let (device, playback) = std::sync::mpsc::sync_channel(1);
    let link = attended_queued_shared(
        label,
        Arc::new(Shared {
            fake_playback: Some(device),
            prime_fake_playback: std::sync::atomic::AtomicBool::new(true),
            ..Shared::default()
        }),
    );
    let events = patient(link.events);
    let session = connected(next(&events));
    frame::write(
        &mut &link.control,
        &ControlToShell::Settings {
            view: settings_view(),
            answer: false,
        },
        Pipe::Control,
    )
    .expect("send Talk-back-off settings");
    let Some(ControlToBrain::QueuedAudioOutput(24_000, output_generation)) =
        frame::read(&mut &link.control, Pipe::Control).expect("read logical registration")
    else {
        panic!("expected the logical queued output");
    };
    acknowledge_queued(&link.control, output_generation);
    assert!(
        matches!(next(&events), LinkEvent::Settings { .. }),
        "the authenticated pair forwards Settings"
    );
    let text = queued_text(1, "write");
    assert!(
        link.handle.send_data(&session, text.clone()),
        "the registered pair accepts text"
    );
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).expect("read the first turn"),
        Some(text),
        "the first typed turn reaches the peer unchanged"
    );
    frame::write(
        &mut &link.control,
        &run_state(
            1,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        ),
        Pipe::Control,
    )
    .expect("name the first typed run");
    assert!(
        matches!(next(&events), LinkEvent::RunState { run: RunId(1), .. }),
        "the output owner binds the first run before its data"
    );
    let mut run = RunId(1);
    match at {
        SilentSpeechAt::Live => {}
        SilentSpeechAt::Stopped => {
            let stop = ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            };
            assert!(
                link.handle.send_control(&session, stop.clone()),
                "the local Stop reaches the peer"
            );
            assert_eq!(
                link.handle.output().stop(session.generation(), run),
                Ok(()),
                "local Stop fences the run without waiting for RunEnd"
            );
            assert_eq!(
                frame::read(&mut &link.control, Pipe::Control).expect("read local Stop"),
                Some(stop),
                "no audio report precedes Stop"
            );
        }
        SilentSpeechAt::Ended => {
            let end = DataToShell::RunEnd { run };
            frame::write(&mut &*link.data, &end, Pipe::Data).expect("send RunEnd");
            let LinkEvent::Data { message, bytes, .. } = next(&events) else {
                panic!("the data reader must finish the run before the first announcement");
            };
            assert_eq!(message, end, "the test observes the ordered RunEnd");
            session.grant(bytes);
            assert_eq!(
                frame::read(&mut &*link.data, Pipe::Data).expect("read credit for RunEnd"),
                Some(DataToBrain::Credit { bytes }),
                "legitimate non-speech data still returns credit"
            );
        }
        SilentSpeechAt::Future => {
            let text = queued_text(2, "next");
            assert!(
                link.handle.send_data(&session, text.clone()),
                "the successor is pending but unnamed"
            );
            assert_eq!(
                frame::read(&mut &*link.data, Pipe::Data).expect("read the second turn"),
                Some(text),
                "the peer knows the successor submit"
            );
            run = RunId(2);
        }
    }
    frame::write(
        &mut &*link.data,
        &DataToShell::SegmentQueued {
            run,
            segment: SegmentId(0),
            step: None,
            start: SegmentStart::Plain,
            output_generation,
        },
        Pipe::Data,
    )
    .expect("send the first unexpected plain announcement");
    link.data.set_timeout(Some(Duration::from_secs(2)));
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data)
            .expect("silent speech must be rejected before the input-ready deadline"),
        None,
        "unexpected speech must close the data pipe without credit"
    );
    queued_pair_closed(link.link, &link.control, &link.data);
    assert!(
        !link
            .handle
            .send_data(&session, queued_text(3, "after refusal")),
        "neither pipe remains available for another turn"
    );
    drop(link.handle);
    assert!(
        matches!(
            playback.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ),
        "the silent pair never tried to prime its fake device"
    );
}

#[test]
fn a_local_queued_stop_the_speaker_cannot_take_leaves_the_pair_open() {
    let link = attended_queued("queued-stop-local");
    let events = patient(link.events);
    let session = connected(next(&events));
    acknowledge_queued(&link.control, queued_registration(&link.control));
    assert!(link.handle.admits_text(Some(&session)));
    assert_eq!(
        link.handle
            .output()
            .stop_locally(session.generation(), RunId(42)),
        Ok(()),
    );
    assert!(session.open(), "a local stop never closes the pair");
    assert!(
        link.handle.admits_text(Some(&session)),
        "and the pair still takes a turn"
    );
    let teardown = thread::spawn(move || link.handle.end(&session));
    queued_pair_closed(link.link, &link.control, &link.data);
    teardown.join().unwrap();
}

#[test]
fn refused_queued_stop_closes_admission_before_background_pair_teardown() {
    use super::output::Denied;

    let link = attended_queued("queued-stop-refused");
    let events = patient(link.events);
    let session = connected(next(&events));
    acknowledge_queued(&link.control, queued_registration(&link.control));
    assert!(link.handle.admits_text(Some(&session)));
    assert_eq!(
        link.handle.output().stop(session.generation(), RunId(42)),
        Err(Denied::Lost),
    );
    assert!(!session.open(), "the old pair cannot accept another send");
    assert!(
        !link
            .handle
            .send_data(&session, queued_text(1, "after failure"))
    );
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control)
        .expect("the OS pipe is still open");
    assert_eq!(
        frame::read(&mut &link.control, Pipe::Control).unwrap(),
        Some(ControlToBrain::Pong)
    );

    let teardown = thread::spawn(move || link.handle.end(&session));
    queued_pair_closed(link.link, &link.control, &link.data);
    teardown.join().unwrap();
}

#[test]
fn queued_replacement_with_unproved_quiet_needs_retry_without_clearing_debt() {
    let _device = serialize_playback();
    let (device, playback) = std::sync::mpsc::sync_channel(1);
    let link = attended_queued_shared(
        "queued-debt-old",
        Arc::new(Shared {
            fake_playback: Some(device),
            ..Shared::default()
        }),
    );
    let events = patient(link.events.clone());
    let (session, output_generation) = queued_named_run(&link, &events);
    let mut playback = stage_queued_pcm(&link.data, &playback, output_generation);
    let deadline = Instant::now() + PATIENCE;
    while !playback.ready() {
        assert!(Instant::now() < deadline, "the old route stages audio");
        thread::sleep(Duration::from_millis(2));
    }
    let mut samples = [0.0; 48];
    playback.render(&mut samples);
    assert_eq!(
        samples.map(f32::to_bits),
        [(9_000.0_f32 / 32_768.0).to_bits(); 48]
    );
    drop(link.control);
    assert!(link.link.join().unwrap());

    assert_refused_queued_replacement(&link.handle);
    let deadline = Instant::now() + PATIENCE;
    while !playback.stopped() {
        assert!(
            Instant::now() < deadline,
            "the lost connection stops the original route"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let mut tail = [99.0; 128];
    playback.render(&mut tail);
    assert!(tail[119..].iter().all(|sample| sample.to_bits() == 0));
    assert_queued_retry_after_local_quiet(&link.handle, &session, "queued-debt-proven");
}

#[test]
fn queued_stop_with_a_late_fade_allows_a_fresh_hold_and_typed_turn() {
    use crate::speaker::DRAIN;
    use kumochi_protocol::message::{GestureId, HoldStatus};

    let _device = serialize_playback();
    let (device, playback) = std::sync::mpsc::sync_channel(1);
    let link = attended_queued_shared(
        "queued-stop-deadline-old",
        Arc::new(Shared {
            fake_playback: Some(device),
            ..Shared::default()
        }),
    );
    let events = patient(link.events.clone());
    let (session, output) = queued_named_run(&link, &events);
    let mut playback = stage_queued_pcm(&link.data, &playback, output);
    let deadline = Instant::now() + PATIENCE;
    while !playback.ready() {
        assert!(Instant::now() < deadline, "the old route stages source");
        thread::sleep(Duration::from_millis(2));
    }
    let run = RunId(7);
    let mut tail = [99.0; 180];
    playback.render_during_copy(&mut tail, |control| {
        frame::write(
            &mut &link.control,
            &ControlToShell::SpeechRevoked { run },
            Pipe::Control,
        )
        .unwrap();
        let limit = Instant::now() + PATIENCE;
        while !control.stopped() {
            assert!(
                Instant::now() < limit,
                "revoke immediately fences new copies"
            );
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(DRAIN + Duration::from_millis(20));
        assert_eq!(control.fenced_handoff(), None);
    });
    assert!(
        tail[..60]
            .iter()
            .all(|frame| frame.to_bits() == (9_000.0_f32 / 32_768.0).to_bits())
    );
    assert_eq!(tail[179].to_bits(), 0);
    assert_eq!(
        frame::read(&mut &link.control, Pipe::Control).unwrap(),
        Some(ControlToBrain::SpeechStopped { run, cursor: None })
    );
    drop(link.control);
    assert!(link.link.join().unwrap());
    assert_queued_retry_after_local_quiet(&link.handle, &session, "queued-stop-deadline-new");

    let hold = attended_shared(
        "late-fade-hold",
        Arc::clone(&link.handle.shared),
        Incarnation(1),
    );
    let events = patient(hold.events);
    let session = connected(next(&events));
    let gesture = GestureId(2);
    let offer = ControlToBrain::VoiceHoldRecognized {
        gesture,
        thread: None,
        selection: 0,
    };
    assert!(hold.handle.send_control(&session, offer.clone()));
    assert_eq!(
        frame::read(&mut &hold.control, Pipe::Control).unwrap(),
        Some(offer)
    );
    frame::write(
        &mut &hold.control,
        &ControlToShell::VoiceHoldStatus {
            gesture,
            status: HoldStatus::Admitted,
        },
        Pipe::Control,
    )
    .unwrap();
    assert!(matches!(
        next(&events),
        LinkEvent::VoiceHold { gesture: id, status: HoldStatus::Admitted, .. } if id == gesture
    ));
    hold.handle.end(&session);
    queued_pair_closed(hold.link, &hold.control, &hold.data);
}

#[test]
fn queued_no_source_disconnect_allows_retry_before_or_after_wire_quiet() {
    for revoke_first in [false, true] {
        let link = attended_queued("queued-no-source-old");
        let events = patient(link.events.clone());
        let (session, _) = queued_named_run(&link, &events);
        assert!(!link.handle.admits_text(Some(&session)));
        if revoke_first {
            frame::write(
                &mut &link.control,
                &ControlToShell::SpeechRevoked { run: RunId(7) },
                Pipe::Control,
            )
            .unwrap();
            assert_eq!(
                frame::read(&mut &link.control, Pipe::Control).unwrap(),
                Some(ControlToBrain::SpeechStopped {
                    run: RunId(7),
                    cursor: None
                })
            );
        }
        drop(link.control);
        assert!(link.link.join().unwrap());
        assert_queued_retry_after_local_quiet(&link.handle, &session, "queued-no-source-retry");
    }
}

fn queued_named_run(
    link: &Attended,
    events: &std::sync::mpsc::Receiver<LinkEvent>,
) -> (Session, kumochi_protocol::message::OutputGeneration) {
    let session = connected(next(events));
    let output = queued_registration(&link.control);
    acknowledge_queued(&link.control, output);
    let text = queued_text(1, "old owner");
    assert!(
        link.handle.send_data(&session, text.clone()),
        "the old pair admits its first turn"
    );
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).expect("the old data pipe"),
        Some(text),
        "the old peer sees the exact typed submit"
    );
    frame::write(
        &mut &link.control,
        &run_state(
            7,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        ),
        Pipe::Control,
    )
    .expect("the old peer names the accepted turn");
    assert!(
        matches!(next(events), LinkEvent::Settings { .. }),
        "settings preceded the run"
    );
    assert!(
        matches!(next(events), LinkEvent::RunState { run: RunId(7), .. }),
        "the Shell bound the old worker"
    );
    (session, output)
}

fn assert_queued_retry_after_local_quiet(old: &LinkHandle, lost: &Session, label: &str) {
    use super::output::{Denied, Retirement};
    use kumochi_protocol::message::GestureId;

    let deadline = Instant::now() + PATIENCE;
    let output = old.output();
    let mut token = loop {
        match output.acquire(GestureId(1)) {
            Ok(token) => break token,
            Err(Denied::Busy | Denied::Inhibited) => {
                assert!(
                    Instant::now() < deadline,
                    "the old run proved quiet locally"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(reason) => panic!("unexpected capture refusal after disconnect: {reason:?}"),
        }
    };
    while let Err((held, reason)) = output.retire(token, Retirement::NoMic) {
        assert_eq!(
            reason,
            Denied::Busy,
            "only brief owner contention may delay retirement"
        );
        assert!(Instant::now() < deadline, "the test inhibit retires");
        token = held;
        thread::sleep(Duration::from_millis(2));
    }
    let next_pair = attended_queued_shared(label, Arc::clone(&old.shared));
    let events = patient(next_pair.events);
    let session = connected(next(&events));
    assert!(
        session.generation() > lost.generation(),
        "Retry admitted a new exact pair"
    );
    acknowledge_queued(&next_pair.control, queued_registration(&next_pair.control));
    assert!(
        !old.send_control(
            lost,
            ControlToBrain::Stop {
                run: RunId(7),
                revealed: Vec::new()
            }
        ),
        "a stale Stop never reaches the replacement"
    );
    old.end(lost);
    let text = queued_text(1, "fresh replacement");
    assert!(
        next_pair.handle.send_data(&session, text.clone()),
        "the new owner accepts fresh intent"
    );
    assert_eq!(
        frame::read(&mut &*next_pair.data, Pipe::Data).expect("the replacement data pipe"),
        Some(text),
        "the old pair's teardown did not close the new one"
    );
    next_pair.handle.end(&session);
    queued_pair_closed(next_pair.link, &next_pair.control, &next_pair.data);
}

#[test]
fn queued_report_refusal_closes_the_full_control_mailbox_and_parked_data_head() {
    use kumochi_protocol::message::{SegmentId, SegmentStart};
    use std::sync::atomic::Ordering;

    let link = attended_queued("queued-report-refused");
    let events = patient(link.events.clone());
    let session = connected(next(&events));
    let output_generation = queued_registration(&link.control);
    acknowledge_queued(&link.control, output_generation);
    let sent = queued_text(1, "old run");
    assert!(link.handle.send_data(&session, sent.clone()));
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).unwrap(),
        Some(sent)
    );
    frame::write(
        &mut &link.control,
        &run_state(
            7,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        ),
        Pipe::Control,
    )
    .unwrap();
    assert!(matches!(next(&events), LinkEvent::Settings { .. }));
    assert!(matches!(
        next(&events),
        LinkEvent::RunState { run: RunId(7), .. }
    ));

    frame::write(
        &mut &*link.data,
        &DataToShell::SegmentQueued {
            run: RunId(8),
            segment: SegmentId(0),
            step: None,
            start: SegmentStart::Plain,
            output_generation,
        },
        Pipe::Data,
    )
    .unwrap();
    link.data.set_timeout(Some(Duration::from_millis(75)));
    assert!(matches!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data),
        Err(frame::FrameError::Io(_))
    ));
    link.data.set_timeout(Some(PATIENCE));
    fill_queued_control_mailbox(&link, &session);

    frame::write(
        &mut &link.control,
        &ControlToShell::SpeechRevoked { run: RunId(7) },
        Pipe::Control,
    )
    .unwrap();
    let limit = Instant::now() + Duration::from_secs(1);
    while session.open() {
        assert!(
            Instant::now() < limit,
            "refused old-run quiet must abort before the data head times out"
        );
        thread::sleep(Duration::from_millis(2));
    }
    queued_pair_closed(link.link, &link.control, &link.data);
    link.handle
        .shared
        .hold_control_writer
        .store(0, Ordering::Release);
    assert_queued_retry_after_local_quiet(&link.handle, &session, "queued-report-refusal-retry");
}

fn fill_queued_control_mailbox(link: &Attended, session: &Session) {
    use super::CONTROL_QUEUE;
    use std::sync::atomic::Ordering;

    let hold = &link.handle.shared.hold_control_writer;
    hold.store(1, Ordering::Release);
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control)
        .expect("trigger the exact control writer");
    let limit = Instant::now() + Duration::from_secs(1);
    while hold.load(Ordering::Acquire) != 2 {
        assert!(
            Instant::now() < limit,
            "the real control writer parks before its next dequeue"
        );
        thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..CONTROL_QUEUE {
        assert!(
            link.handle.send_control(session, ControlToBrain::Pong),
            "the parked mailbox accepts its bounded capacity"
        );
    }
    assert!(
        !link.handle.send_control(session, ControlToBrain::Pong),
        "the mailbox is saturated before reporting"
    );
}

#[test]
fn queued_refused_progress_preserves_only_the_original_callback_quiet() {
    let _device = serialize_playback();
    let (handle, lost, mut playback) = refused_queued_progress("queued-refused-progress");
    assert_refused_queued_replacement(&handle);

    let mut tail = [99.0; 128];
    playback.render(&mut tail);
    assert!(
        tail[0] > 0.0,
        "the original callback renders a real stop ramp"
    );
    assert!(
        tail[119..].iter().all(|sample| sample.to_bits() == 0),
        "the original ramp reaches silence"
    );
    assert_queued_retry_after_local_quiet(&handle, &lost, "queued-refused-progress-proven");
    playback.render(&mut tail);
    assert_eq!(
        tail.map(f32::to_bits),
        [0; 128],
        "no old suffix after quiet"
    );
}

#[test]
fn talk_back_reconnects_do_not_spend_or_reset_the_loss_budget() {
    use super::LinkState;
    use kumochi_protocol::message::AudioSession;

    let link = Link {
        audio: AudioSession::QueuedRequired,
        ..bare_link()
    };
    let (events, states) = async_channel::unbounded();
    let (retry, retries) = async_channel::bounded::<()>(1);
    drop(retry);
    let genuine_loss = Some(Instant::now());
    for _ in 0..2 {
        assert_eq!(
            link.after_loss(&events, &retries, Attendance::TalkBackChanged, genuine_loss),
            Some((LinkState::Reconnecting, genuine_loss))
        );
    }
    assert!(states.is_empty());
    assert_eq!(
        link.after_loss(&events, &retries, Attendance::Lost, genuine_loss),
        None
    );
    assert!(matches!(
        states.try_recv(),
        Ok(LinkEvent::State(LinkState::Failed))
    ));
}

#[test]
fn queued_refused_progress_without_tail_becomes_unavailable() {
    use super::{LinkState, output::RefusedTail};
    use kumochi_protocol::message::AudioSession;

    let _device = serialize_playback();
    let (handle, _lost, mut playback) = refused_queued_progress("queued-unproved-progress");
    let link = Link {
        audio: AudioSession::QueuedRequired,
        shared: Arc::clone(&handle.shared),
        ..bare_link()
    };
    let (events, states) = async_channel::unbounded();
    let (retry, retries) = async_channel::bounded(1);
    retry
        .try_send(())
        .expect("queue a Retry before any quiet proof");
    assert_eq!(
        link.after_loss(&events, &retries, Attendance::Lost, None),
        None,
        "terminal loss does not choose a reconnect state"
    );
    assert!(
        matches!(
            states.try_recv(),
            Ok(LinkEvent::State(LinkState::OutputUnavailable))
        ),
        "the normal link loop publishes terminal output status"
    );
    assert!(
        matches!(states.try_recv(), Err(async_channel::TryRecvError::Empty)),
        "terminal output publishes no Retry state"
    );
    assert_eq!(
        retries.try_recv(),
        Ok(()),
        "terminal loss ignores even buffered Retry"
    );
    playback.render(&mut [0.0; 128]);
    assert_eq!(
        handle.output().wait_for_refused_tail(),
        Some(RefusedTail::Unavailable),
        "a callback delivered after the bound cannot reopen admission"
    );
    assert_refused_queued_replacement(&handle);
}

fn refused_queued_progress(label: &str) -> (LinkHandle, Session, FixturePlayback) {
    use kumochi_protocol::message::{PROGRESS_INTERVAL_MS, SegmentId, SpeechRetirement};
    use std::sync::atomic::Ordering;

    let (device, opened) = std::sync::mpsc::sync_channel(1);
    let link = attended_queued_shared(
        label,
        Arc::new(Shared {
            fake_playback: Some(device),
            ..Shared::default()
        }),
    );
    let events = patient(link.events.clone());
    let (session, output_generation) = queued_named_run(&link, &events);
    fill_queued_control_mailbox(&link, &session);
    let mut playback = stage_queued_pcm(&link.data, &opened, output_generation);
    let start = Instant::now();
    for _ in 0..=PROGRESS_INTERVAL_MS / 20 {
        assert!(
            session.open(),
            "the mailbox remains full below the progress threshold"
        );
        while !playback.ready() {
            assert!(
                start.elapsed() < PATIENCE,
                "the original worker stages real PCM"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let mut frames = [0.0; 480];
        playback.render(&mut frames);
        assert_eq!(
            frames.map(f32::to_bits),
            [(9_000.0_f32 / 32_768.0).to_bits(); 480],
            "all callback frames come from the original nonzero source"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while session.open() {
        assert!(
            Instant::now() < deadline,
            "refused non-final progress aborts the old pair"
        );
        thread::sleep(Duration::from_millis(2));
    }
    while !playback.stopped() {
        assert!(
            Instant::now() < deadline,
            "refused non-final progress fences the original callback"
        );
        thread::sleep(Duration::from_millis(2));
    }
    queued_pair_closed(link.link, &link.control, &link.data);
    let receipt = link
        .handle
        .shared
        .refused_retirement
        .lock()
        .expect("the fixture report recorder is healthy")
        .take()
        .expect("the first refusal was SpeechRetired");
    assert_eq!(
        receipt,
        SpeechRetirement {
            run: RunId(7),
            output: output_generation,
            segment: SegmentId(0),
            retired: 6_240,
            heard: 6_240,
            final_segment: false,
        },
        "the full mailbox refused non-final retirement for the original callback"
    );
    link.handle
        .shared
        .hold_control_writer
        .store(0, Ordering::Release);
    (link.handle, session, playback)
}

fn stage_queued_pcm(
    mut data: &PipeClient,
    opened: &std::sync::mpsc::Receiver<FixturePlayback>,
    output_generation: kumochi_protocol::message::OutputGeneration,
) -> FixturePlayback {
    use kumochi_protocol::message::{SegmentId, SegmentStart, SpeechPcm};
    use std::io::Write;

    let (run, segment) = (RunId(7), SegmentId(0));
    let announcement = DataToShell::SegmentQueued {
        run,
        segment,
        step: None,
        start: SegmentStart::Plain,
        output_generation,
    };
    let pcm = (0..5).map(|seq| {
        DataToShell::SpeechPcm(SpeechPcm {
            run,
            segment,
            output_generation,
            seq,
            samples: vec![9_000; if seq == 0 { 480 } else { 2_400 }],
            last: None,
        })
    });
    let mut source = None;
    let mut total = 0;
    for (at, message) in std::iter::once(announcement).chain(pcm).enumerate() {
        let bytes = frame::encode(&message, Pipe::Data).expect("bounded ingress frame");
        total += u32::try_from(bytes.len()).expect("bounded frame length");
        data.write_all(&bytes)
            .expect("the original data pipe accepts PCM");
        if at == 1 {
            source = Some(
                opened
                    .recv_timeout(PATIENCE)
                    .expect("the worker opens while still priming"),
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    let mut credited = 0;
    while credited < total {
        let Some(DataToBrain::Credit { bytes }) =
            frame::read(&mut data, Pipe::Data).expect("read the original data pipe")
        else {
            panic!("the original pair returns the staged ingress credit");
        };
        assert!(bytes > 0, "each ingress credit makes progress");
        credited += bytes;
    }
    assert_eq!(
        credited, total,
        "all staged ingress is credited before holding the tail"
    );
    source.expect("the original callback opened")
}

#[test]
fn queued_reconnect_waits_for_the_exact_microphone_retirement() {
    use super::output::{Denied, Retirement};
    use kumochi_protocol::message::GestureId;
    use windows::Win32::Foundation::ERROR_TIMEOUT;

    let (link, inhibit, reserved) = lost_quiet_mic();
    let (control, _data, connection) = replacement_pair("mic-wait");
    frame::write(
        &mut &control,
        &ControlToShell::Settings {
            view: settings_view(),
            answer: false,
        },
        Pipe::Control,
    )
    .unwrap();
    let (events, received) = async_channel::unbounded();
    let (handle, start) = start(link, events);
    let output = handle.output();

    let foreign = super::output::OutputOwner::default()
        .acquire(GestureId(93))
        .unwrap();
    assert_eq!(
        output.retire(foreign, Retirement::NoMic).unwrap_err().1,
        Denied::Stale
    );
    let (inhibit, denied) = output.retire(inhibit, Retirement::Retired).unwrap_err();
    assert_eq!(denied, Denied::Inhibited, "cancel is not retirement");
    let reconnect_by = Instant::now() + super::CONNECT_TIMEOUT;
    let worker = thread::spawn(move || {
        start
            .link
            .attend_until(connection, &start.events, reconnect_by)
    });
    control.set_timeout(Some(Duration::from_millis(500)));
    let early = frame::read::<ControlToBrain>(&mut &control, Pipe::Control);
    assert!(
        matches!(&early, Err(frame::FrameError::Io(error)) if error.raw_os_error() == Some(ERROR_TIMEOUT.to_hresult().0)),
        "the pair waits without registering or closing: {early:?}"
    );
    assert!(received.is_empty());
    assert!(handle.shared.outbox.lock().unwrap().is_none());

    let received = patient(received);
    drop(reserved);
    *handle.shared.microphone_stop.lock().unwrap() = Some(super::microphone::Stop::Received {
        gesture: GestureId(1),
        sent_on: Some(1),
    });
    let mut inhibit = inhibit;
    loop {
        match output.retire(inhibit, Retirement::Retired) {
            Ok(()) => break,
            Err((token, Denied::Busy)) => {
                let left = reconnect_by.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "the exact retired microphone releases its inhibit before reconnect expires"
                );
                inhibit = token;
                thread::sleep(Duration::from_millis(10).min(left));
            }
            Err((_, denied)) => panic!("the exact retired microphone was refused: {denied:?}"),
        }
    }
    let session = connected(next(&received));
    assert_eq!(session.generation(), 2);
    let LinkEvent::Settings { generation, .. } = next(&received) else {
        panic!("the recovered pair publishes its settings");
    };
    assert_eq!(generation, session.generation());
    control.set_timeout(Some(PATIENCE));
    assert_eq!(
        frame::read(&mut &control, Pipe::Control).unwrap(),
        Some(ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1)
        }),
        "replay only the exact retired microphone's metadata"
    );
    frame::write(
        &mut &control,
        &ControlToShell::VoiceHoldStopAccepted {
            gesture: GestureId(1),
        },
        Pipe::Control,
    )
    .unwrap();
    let Some(ControlToBrain::QueuedAudioOutput(_, key)) =
        frame::read(&mut &control, Pipe::Control).unwrap()
    else {
        panic!("the recovered pair starts its own registration");
    };
    acknowledge_queued(&control, key);
    assert!(handle.admits_text(Some(&session)));
    assert!(!handle.voice_stop_pending());
    handle.end(&session);
    assert_eq!(worker.join().unwrap(), Attendance::Lost);
}

#[test]
fn queued_reconnect_expires_without_retiring_an_unproved_microphone() {
    use super::output::{Denied, Retirement};
    use kumochi_protocol::message::GestureId;

    let (link, inhibit, reserved) = lost_quiet_mic();
    let (control, data, connection) = replacement_pair("mic-expired");
    let (events, received) = async_channel::unbounded();
    let deadline = Instant::now() + Duration::from_millis(100);
    assert_eq!(
        link.attend_until(connection, &events, deadline),
        Attendance::Retry
    );
    assert!(Instant::now() >= deadline);
    assert!(received.is_empty());
    assert!(link.shared.outbox.lock().unwrap().is_none());
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &control, Pipe::Control).unwrap(),
        None
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &data, Pipe::Data).unwrap(),
        None
    );
    let output = &link.shared.output;
    assert_eq!(output.acquire(GestureId(2)), Err(Denied::Inhibited));
    let (inhibit, denied) = output.retire(inhibit, Retirement::Retired).unwrap_err();
    assert_eq!(denied, Denied::Inhibited);
    drop(reserved);
    output.retire(inhibit, Retirement::Retired).unwrap();
}

#[test]
fn queued_reconnect_rejects_a_successful_late_handshake_or_late_attendance() {
    use super::output::Retirement;

    for at_connector in [true, false] {
        let (link, inhibit, reserved) = lost_quiet_mic();
        let label = if at_connector {
            "late-hello"
        } else {
            "late-attend"
        };
        let (control, data, shell_control, shell_data) = queued_hello_pipes(label);
        let (events, received) = async_channel::unbounded();
        let (handle, start) = start(link, events);
        let worker = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(1);
            let connection = start.link.connect_with(
                end,
                queued_fixture_connector(shell_control, at_connector.then_some(end), || {
                    Ok(shell_data)
                }),
            );
            if at_connector {
                assert!(
                    matches!(connection, Err(super::LinkError::ConnectExpired)),
                    "the real connector did not refuse a successful late handshake: {connection:?}"
                );
                return Attendance::Retry;
            }
            let connection =
                connection.expect("the handshake completed before the attempt deadline");
            start
                .link
                .attend_until(connection, &start.events, Instant::now())
        });
        let output = handle.output();
        drop(reserved);
        output.retire(inhibit, Retirement::Retired).unwrap();
        read_queued_fixture_hello(&control, &data);
        if !at_connector {
            let _offered = frame::write(
                &mut &control,
                &ControlToShell::Settings {
                    view: settings_view(),
                    answer: false,
                },
                Pipe::Control,
            );
        }
        let next = frame::read::<ControlToBrain>(&mut &control, Pipe::Control).unwrap();
        assert_eq!(next, None, "the expired pair sends no work");
        assert_eq!(
            frame::read::<DataToBrain>(&mut &data, Pipe::Data).unwrap(),
            None
        );
        assert!(received.is_empty(), "the expired pair is never published");
        assert!(handle.shared.outbox.lock().unwrap().is_none());
        assert_eq!(worker.join().unwrap(), Attendance::Retry);
    }
}

#[test]
fn queued_reconnect_cuts_off_a_stalled_real_welcome_payload() {
    use std::io::Write as _;
    use windows::Win32::Foundation::ERROR_TIMEOUT;

    let (control, data, shell_control, shell_data) = queued_hello_pipes("stall-welcome");
    let link = Link {
        audio: super::AudioSession::QueuedRequired,
        ..bare_link()
    };
    let (events, received) = async_channel::unbounded();
    let (handle, start) = start(link, events);
    handle.delete(ThreadId(11));
    let (finished, outcome) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let mut handshake = queued_fixture_connector(shell_control, None, || {
            drop(shell_data);
            panic!("an incomplete Welcome cannot open the data leg")
        });
        let end = Instant::now() + Duration::from_millis(750);
        let result = start.link.connect_with(end, |attempt| {
            let result = handshake(attempt);
            assert!(
                matches!(
                    &result,
                    Err(super::LinkError::Frame(frame::FrameError::Io(error)))
                        if error.raw_os_error() == Some(ERROR_TIMEOUT.to_hresult().0)
                ),
                "the actual blocked pipe transfer timed out: {result:?}"
            );
            result
        });
        finished.send(result).unwrap();
    });
    assert!(matches!(
        frame::read(&mut &control, Pipe::Control).unwrap(),
        Some(ControlToBrain::QueuedHello(_))
    ));
    let welcome = frame::encode(&queued_fixture_welcome(), Pipe::Control).unwrap();
    (&control).write_all(&welcome[..frame::HEADER_LEN]).unwrap();
    assert!(handle.begin_quit());
    assert!(
        matches!(
            outcome.recv_timeout(Duration::from_secs(2)),
            Ok(Err(super::LinkError::Quitting))
        ),
        "the stalled leg must not consume a fresh five-second timeout"
    );
    worker.join().unwrap();
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &control, Pipe::Control).unwrap(),
        None
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &data, Pipe::Data).unwrap(),
        None
    );
    assert!(received.is_empty());
    assert!(handle.shared.outbox.lock().unwrap().is_none());
}

#[test]
fn queued_reconnect_does_not_start_a_handshake_at_or_after_the_deadline() {
    let link = Link {
        audio: super::AudioSession::QueuedRequired,
        ..bare_link()
    };
    assert!(matches!(
        link.connect_with(Instant::now(), |_| panic!("the attempt is already expired")),
        Err(super::LinkError::ConnectExpired)
    ));
    let end = Instant::now() + super::RETRY_DELAY;
    let mut calls = 0;
    let result = link.connect_with(end, |attempt| {
        assert_eq!(attempt, Some(end));
        calls += 1;
        assert_eq!(calls, 1, "the retry delay has used the remaining budget");
        Err(super::LinkError::Busy)
    });
    assert!(matches!(result, Err(super::LinkError::ConnectExpired)));
}

#[test]
fn queued_reconnect_can_be_observed_after_the_timely_admission_decision() {
    use super::output::Retirement;

    let (link, inhibit, reserved) = lost_quiet_mic();
    let (control, data, shell_control, shell_data) = queued_hello_pipes("observed-late");
    let (events, received) = async_channel::unbounded();
    let (handle, start) = start(link, events);
    drop(reserved);
    handle
        .output()
        .retire(inhibit, Retirement::Retired)
        .unwrap();
    handle.delete(ThreadId(11));
    let end = Instant::now() + Duration::from_secs(1);
    let worker = thread::spawn(move || {
        let connection = start
            .link
            .connect_with(
                end,
                queued_fixture_connector(shell_control, None, || Ok(shell_data)),
            )
            .unwrap();
        let result = start.link.attend_until(connection, &start.events, end);
        drop(start.alive);
        result
    });
    read_queued_fixture_hello(&control, &data);
    frame::write(
        &mut &control,
        &ControlToShell::Settings {
            view: settings_view(),
            answer: false,
        },
        Pipe::Control,
    )
    .unwrap();
    let Some(ControlToBrain::QueuedAudioOutput(_, key)) =
        frame::read(&mut &control, Pipe::Control).unwrap()
    else {
        panic!("the admitted pair began registration before the UI consumed Connected");
    };
    thread::sleep(end.saturating_duration_since(Instant::now()));
    assert!(Instant::now() >= end);
    let session = connected(received.try_recv().unwrap());
    assert_eq!(session.generation(), 2);
    assert_eq!(
        session.issue_session(),
        IssueSession {
            incarnation: Incarnation(1),
            epoch: 2
        }
    );
    assert!(!handle.admits_text(Some(&session)));
    acknowledge_queued(&control, key);
    assert!(
        handle.admits_text(Some(&session)),
        "registration has its own deadline"
    );
    assert!(handle.begin_quit());
    let finish = thread::spawn(move || handle.finish_quit());
    assert_eq!(
        frame::read(&mut &control, Pipe::Control).unwrap(),
        Some(ControlToBrain::DeleteThread {
            thread: ThreadId(11),
            after_turns: 0,
            resend: false
        })
    );
    assert_eq!(
        frame::read(&mut &control, Pipe::Control).unwrap(),
        Some(ControlToBrain::Quitting)
    );
    drop(control);
    assert_eq!(worker.join().unwrap(), Attendance::Lost);
    finish.join().unwrap();
}

#[test]
fn queued_reconnect_quits_its_authenticated_parked_pair_after_held_deletes() {
    use super::output::{Denied, Retirement};
    use kumochi_protocol::message::GestureId;
    use windows::Win32::Foundation::ERROR_TIMEOUT;

    let (link, inhibit, reserved) = lost_quiet_mic();
    let (control, data, shell_control, shell_data) = queued_hello_pipes("parked-quit");
    let (events, received) = async_channel::unbounded();
    let (handle, start) = start(link, events);
    handle.delete(ThreadId(11));
    handle.delete(ThreadId(12));
    handle.shared.deletes()[0].sent_on = Some(1);
    *handle.shared.microphone_stop.lock().unwrap() = Some(super::microphone::Stop::Received {
        gesture: GestureId(1),
        sent_on: Some(1),
    });
    let worker = thread::spawn(move || {
        let end = Instant::now() + PATIENCE;
        let connection = start
            .link
            .connect_with(
                end,
                queued_fixture_connector(shell_control, None, || Ok(shell_data)),
            )
            .unwrap();
        start.link.attend_until(connection, &start.events, end)
    });
    read_queued_fixture_hello(&control, &data);
    control.set_timeout(Some(Duration::from_millis(250)));
    assert!(
        matches!(frame::read::<ControlToBrain>(&mut &control, Pipe::Control),
        Err(frame::FrameError::Io(error)) if error.raw_os_error() == Some(ERROR_TIMEOUT.to_hresult().0))
    );
    assert!(handle.shared.outbox.lock().unwrap().is_none());
    assert!(handle.begin_quit());
    control.set_timeout(Some(PATIENCE));
    for (thread, resend) in [(ThreadId(11), true), (ThreadId(12), false)] {
        assert_eq!(
            frame::read(&mut &control, Pipe::Control).unwrap(),
            Some(ControlToBrain::DeleteThread {
                thread,
                after_turns: 0,
                resend
            })
        );
    }
    assert_eq!(
        frame::read(&mut &control, Pipe::Control).unwrap(),
        Some(ControlToBrain::Quitting)
    );
    assert_eq!(worker.join().unwrap(), Attendance::Ended);
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &control, Pipe::Control).unwrap(),
        None
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &data, Pipe::Data).unwrap(),
        None
    );
    assert!(received.is_empty());
    assert!(handle.shared.outbox.lock().unwrap().is_none());
    assert!(
        handle.voice_stop_pending(),
        "Quit must not settle the old Stop"
    );
    assert_eq!(
        handle.output().acquire(GestureId(2)),
        Err(Denied::Inhibited)
    );
    let (inhibit, denied) = handle
        .output()
        .retire(inhibit, Retirement::Retired)
        .unwrap_err();
    assert_eq!(denied, Denied::Inhibited);
    drop(reserved);
    handle
        .output()
        .retire(inhibit, Retirement::Retired)
        .unwrap();
}

#[test]
fn queued_reconnect_does_not_send_shutdown_to_unknown_or_partial_pairs() {
    for greeted in [false, true] {
        let label = if greeted {
            "partial-quit"
        } else {
            "unknown-quit"
        };
        let (control, data, shell_control, shell_data) = queued_hello_pipes(label);
        let link = Link {
            audio: super::AudioSession::QueuedRequired,
            ..bare_link()
        };
        let (events, received) = async_channel::unbounded();
        let (handle, start) = start(link, events);
        handle.delete(ThreadId(11));
        let worker = thread::spawn(move || {
            let end = Instant::now() + PATIENCE;
            start.link.connect_with(
                end,
                queued_fixture_connector(shell_control, None, || {
                    drop(shell_data);
                    Err(super::LinkError::NotOurBrain(PathBuf::from("foreign")))
                }),
            )
        });
        assert!(matches!(
            frame::read(&mut &control, Pipe::Control).unwrap(),
            Some(ControlToBrain::QueuedHello(_))
        ));
        assert!(handle.begin_quit());
        let reply = if greeted {
            queued_fixture_welcome()
        } else {
            ControlToShell::Ping
        };
        frame::write(&mut &control, &reply, Pipe::Control).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &control, Pipe::Control).unwrap(),
            None
        );
        assert_eq!(
            frame::read::<DataToBrain>(&mut &data, Pipe::Data).unwrap(),
            None
        );
        assert!(received.is_empty());
        assert!(handle.shared.outbox.lock().unwrap().is_none());
    }
}

fn queued_fixture_connector(
    control: PipeClient,
    wait_through: Option<Instant>,
    open_data: impl FnOnce() -> Result<PipeClient, super::LinkError>,
) -> impl FnMut(Option<Instant>) -> Result<Connection, super::LinkError> {
    let mut opening = Some((control, open_data));
    move |attempt| {
        let (control, open_data) = opening.take().expect("one fixture attempt");
        let connection = super::handshake_pair(
            control,
            queued_fixture_hello(),
            super::AudioSession::QueuedRequired,
            attempt,
            open_data,
        )?;
        if let Some(end) = wait_through {
            thread::sleep(end.saturating_duration_since(Instant::now()));
        }
        Ok(connection)
    }
}

fn queued_hello_pipes(label: &str) -> (PipeClient, PipeClient, PipeClient, PipeClient) {
    let (control, shell_control) = pair(&format!("queued-reconnect-{label}-c"));
    let (data, shell_data) = pair(&format!("queued-reconnect-{label}-d"));
    for pipe in [&control, &data, &shell_control, &shell_data] {
        pipe.set_timeout(Some(PATIENCE));
    }
    (control, data, shell_control, shell_data)
}

fn queued_fixture_hello() -> kumochi_protocol::message::Hello {
    kumochi_protocol::message::Hello {
        build_id: kumochi_protocol::BUILD_ID.into(),
        capability: kumochi_protocol::Secret::new([7; kumochi_protocol::message::CAPABILITY_LEN]),
    }
}

fn queued_fixture_welcome() -> ControlToShell {
    ControlToShell::Welcome {
        incarnation: Incarnation(1),
        issue_epoch: 2,
    }
}

fn read_queued_fixture_hello(control: &PipeClient, data: &PipeClient) {
    assert!(
        matches!(
            frame::read(&mut &*control, Pipe::Control).expect("control hello"),
            Some(ControlToBrain::QueuedHello(_))
        ),
        "expected a queued control hello"
    );
    frame::write(&mut &*control, &queued_fixture_welcome(), Pipe::Control).expect("Welcome");
    assert!(
        matches!(
            frame::read(&mut &*data, Pipe::Data).expect("data hello"),
            Some(DataToBrain::QueuedHello(
                _,
                IssueSession {
                    incarnation: Incarnation(1),
                    epoch: 2
                }
            ))
        ),
        "the data hello names this exact pair"
    );
}

fn lost_quiet_mic() -> (
    Link,
    super::output::InhibitToken,
    crate::platform::microphone::Reserved,
) {
    use kumochi_protocol::message::{AudioSession, GestureId};
    use std::sync::atomic::Ordering;

    let link = Link {
        audio: AudioSession::QueuedRequired,
        ..bare_link()
    };
    link.shared.generations.store(1, Ordering::Release);
    let output = &link.shared.output;
    output.begin_quiet(1);
    let gesture = GestureId(1);
    let mic = link
        .shared
        .microphone
        .arm(gesture)
        .expect("arm the old mic");
    let inhibit = output.acquire(gesture).expect("acquire the quiet inhibit");
    output.bind_mic(&inhibit, &mic).expect("bind the old mic");
    let claim = output.claim(&inhibit, 1).expect("claim the quiet output");
    let reserved = output.start_mic(&claim).expect("reserve without opening");
    output.end(1);
    (link, inhibit, reserved)
}

fn replacement_pair(label: &str) -> (PipeClient, PipeClient, Connection) {
    let (control, shell_control) = pair(&format!("queued-replacement-{label}-c"));
    let (data, shell_data) = pair(&format!("queued-replacement-{label}-d"));
    control.set_timeout(Some(PATIENCE));
    data.set_timeout(Some(PATIENCE));
    (
        control,
        data,
        Connection {
            control: Arc::new(shell_control),
            data: Arc::new(shell_data),
            issue_origin: IssueSession {
                incarnation: Incarnation(1),
                epoch: 2,
            },
        },
    )
}

fn assert_refused_queued_replacement(handle: &LinkHandle) {
    use super::output::Denied;
    use kumochi_protocol::message::{AudioSession, GestureId};

    let (control, shell_control) = pair("queued-refused-tail-control");
    let (data, shell_data) = pair("queued-refused-tail-data");
    control.set_timeout(Some(PATIENCE));
    data.set_timeout(Some(PATIENCE));
    let (sender, receiver) = async_channel::unbounded();
    let link = Link {
        audio: AudioSession::QueuedRequired,
        shared: Arc::clone(&handle.shared),
        ..bare_link()
    };
    assert_eq!(
        link.attend(
            Connection {
                control: Arc::new(shell_control),
                data: Arc::new(shell_data),
                issue_origin: IssueSession {
                    incarnation: Incarnation(1),
                    epoch: 93
                },
            },
            &sender
        ),
        Attendance::Retry,
        "the low-level pair cannot install an owner while local quiet is unproved"
    );
    assert!(
        matches!(receiver.try_recv(), Err(async_channel::TryRecvError::Empty)),
        "no connected state escaped the admission check"
    );
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &control, Pipe::Control)
            .expect("read replacement control"),
        None,
        "no old speech report and no new registration on the refused pair"
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &data, Pipe::Data).expect("read replacement data"),
        None,
        "the refused pair has no data admission"
    );
    assert!(
        matches!(
            handle.output().acquire(GestureId(1)),
            Err(Denied::Inhibited)
        ),
        "capture remains fenced until the exact callback proves its tail"
    );
}

#[test]
fn queued_fresh_intent_and_full_enqueue_do_not_replay_or_reserve_a_turn() {
    use super::DataOut;

    let link = attended_queued("queued-intent");
    let events = patient(link.events);
    let session = connected(next(&events));
    let output = queued_registration(&link.control);
    assert!(
        !link.handle.admits_text(Some(&session)),
        "registration is not an ACK"
    );
    assert!(!link.handle.send_data(&session, queued_text(1, "early")));
    acknowledge_queued(&link.control, output);
    assert!(link.handle.admits_text(Some(&session)));

    let (full, blocked) = async_channel::bounded(1);
    assert!(full.try_send(DataOut::Credit).is_ok());
    let data = {
        let mut outbox = link.handle.shared.outbox.lock().unwrap();
        std::mem::replace(&mut outbox.as_mut().unwrap().data, full)
    };
    assert!(!link.handle.send_data(&session, queued_text(1, "full")));
    assert!(matches!(blocked.try_recv(), Ok(DataOut::Credit)));
    assert!(matches!(
        blocked.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    assert!(
        link.handle.admits_text(Some(&session)),
        "failed enqueue must not reserve a turn"
    );
    link.handle
        .shared
        .outbox
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .data = data;

    assert!(link.handle.send_data(&session, queued_text(1, "fresh")));
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).unwrap(),
        Some(queued_text(1, "fresh"))
    );
    assert!(
        !link.handle.admits_text(Some(&session)),
        "successful enqueue reserves exactly one turn"
    );
    assert!(!link.handle.send_data(&session, queued_text(2, "overlap")));
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one attended pair covers control progress, exact head credit, and its late PCM"
)]
fn queued_plain_head_waits_for_the_exact_named_run_without_blocking_control() {
    use kumochi_protocol::message::{SegmentId, SegmentStart, SpeechPcm};

    let link = attended_queued("queued-bind-head");
    let events = patient(link.events);
    let session = connected(next(&events));
    let output_generation = queued_registration(&link.control);
    assert!(matches!(next(&events), LinkEvent::Settings { .. }));
    acknowledge_queued(&link.control, output_generation);
    let head = DataToShell::SegmentQueued {
        run: RunId(8),
        segment: SegmentId(0),
        step: None,
        start: SegmentStart::Plain,
        output_generation,
    };
    let bytes = u32::try_from(frame::encode(&head, Pipe::Data).expect("frame").len()).unwrap();
    frame::write(&mut &*link.data, &head, Pipe::Data).expect("early head");
    link.data.set_timeout(Some(Duration::from_millis(75)));
    let no_credit = || {
        let outcome = frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data);
        assert!(
            matches!(outcome, Err(frame::FrameError::Io(_))),
            "the read must time out, not return a frame or EOF: {outcome:?}"
        );
    };
    no_credit();
    frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read(&mut &link.control, Pipe::Control).unwrap(),
        Some(ControlToBrain::Pong)
    );
    for (turn, run) in [(1, 7), (2, 8)] {
        let text = queued_text(turn, "name this run");
        assert!(link.handle.send_data(&session, text.clone()));
        assert_eq!(
            frame::read(&mut &*link.data, Pipe::Data).unwrap(),
            Some(text)
        );
        frame::write(
            &mut &link.control,
            &run_state(
                run,
                ThreadId(1),
                Phase::Thinking,
                Some(RunOrigin::Typed { turn }),
            ),
            Pipe::Control,
        )
        .expect("name");
        assert!(
            matches!(next(&events), LinkEvent::RunState { run: RunId(actual), .. } if actual == run)
        );
        if run == 7 {
            no_credit();
            frame::write(
                &mut &link.control,
                &ControlToShell::SpeechRevoked { run: RunId(7) },
                Pipe::Control,
            )
            .expect("stop the first run");
            assert!(matches!(
                frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
                Some(ControlToBrain::SpeechStopped { run: RunId(7), .. })
            ));
        }
    }
    link.data.set_timeout(Some(PATIENCE));
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).unwrap(),
        Some(DataToBrain::Credit { bytes })
    );
    link.data.set_timeout(Some(Duration::from_millis(75)));
    no_credit();
    frame::write(
        &mut &link.control,
        &ControlToShell::SpeechRevoked { run: RunId(8) },
        Pipe::Control,
    )
    .expect("revoke the bound run");
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
        Some(ControlToBrain::SpeechStopped {
            run: RunId(8),
            cursor: None
        })
    );
    let late = DataToShell::SpeechPcm(SpeechPcm {
        run: RunId(8),
        segment: SegmentId(0),
        output_generation,
        seq: 0,
        samples: Vec::new(),
        last: None,
    });
    link.data.set_timeout(Some(PATIENCE));
    frame::write(&mut &*link.data, &late, Pipe::Data).expect("late already-charged PCM");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
        Some(DataToBrain::Credit {
            bytes: u32::try_from(frame::encode(&late, Pipe::Data).unwrap().len()).unwrap(),
        })
    );
    drop(link.control);
    assert!(link.link.join().expect("joined"));
}

#[test]
fn queued_wrong_or_duplicate_registration_ack_ends_the_pair() {
    use kumochi_protocol::message::OutputGeneration;

    for duplicate in [false, true] {
        let link = attended_queued(if duplicate {
            "queued-ack-duplicate"
        } else {
            "queued-ack-wrong"
        });
        let events = patient(link.events);
        let _session = connected(next(&events));
        let output = queued_registration(&link.control);
        if duplicate {
            frame::write(
                &mut &link.control,
                &ControlToShell::QueuedAudioRegistered(output),
                Pipe::Control,
            )
            .expect("first ack");
        }
        let invalid = if duplicate {
            output
        } else {
            OutputGeneration(output.0 + 1)
        };
        frame::write(
            &mut &link.control,
            &ControlToShell::QueuedAudioRegistered(invalid),
            Pipe::Control,
        )
        .expect("invalid ack");
        queued_pair_closed(link.link, &link.control, &link.data);
    }
}

#[test]
fn queued_speech_start_or_scene_promise_closes_without_data_credit() {
    use kumochi_protocol::message::{SceneId, SegmentId, SegmentStart, SpeechPromise};
    use kumochi_scene::{Record, parse_record};

    let Record::Step(step) = parse_record(br#"{"step":{"id":"speech-step"}}"#).unwrap() else {
        panic!("step");
    };
    for promise in [false, true] {
        let link = attended_queued(if promise {
            "queued-promise"
        } else {
            "queued-speech-start"
        });
        let events = patient(link.events);
        let _session = connected(next(&events));
        let output = queued_registration(&link.control);
        frame::write(
            &mut &link.control,
            &ControlToShell::QueuedAudioRegistered(output),
            Pipe::Control,
        )
        .expect("registered");
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).expect("ping");
        assert_eq!(
            frame::read(&mut &link.control, Pipe::Control).unwrap(),
            Some(ControlToBrain::Pong)
        );
        let segment = SegmentId(0);
        let run = RunId(8);
        let unsupported = if promise {
            DataToShell::SceneStepCommit {
                run,
                scene: SceneId(1),
                index: 0,
                step: step.clone(),
                speech: Some(SpeechPromise {
                    output_generation: output,
                    segment,
                }),
            }
        } else {
            DataToShell::SegmentQueued {
                run,
                segment,
                step: None,
                start: SegmentStart::Speech,
                output_generation: output,
            }
        };
        frame::write(&mut &*link.data, &unsupported, Pipe::Data).expect("unsupported frame");
        queued_pair_closed(link.link, &link.control, &link.data);
        assert!(matches!(next(&events), LinkEvent::Settings { .. }));
        assert!(matches!(
            events.recv_timeout(PATIENCE),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}

#[cfg(debug_assertions)]
fn narrated_attended(label: &str) -> Attended {
    attended_queued_shared(
        label,
        Arc::new(super::Shared {
            narrated: true,
            ..super::Shared::default()
        }),
    )
}

#[cfg(debug_assertions)]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one pair through delayed delivery, retirement, successor, and live duplicate"
)]
fn narrated_absence_survives_delayed_delivery_and_reclaims_retired_scenes() {
    use crate::presenter::{Presenter, fixture};
    use crate::speaker::conductor::Assignment;
    use kumochi_protocol::message::{SceneId, SegmentId, SegmentStart, SpeechPromise, StepRef};
    use std::io::Write as _;

    let link = narrated_attended("narrated-absence-retirement");
    let events = patient(link.events.clone());
    let (session, output) = queued_named_run(&link, &events);
    let generation = session.generation();
    let now = Instant::now();
    let mut presenter = fixture::presenter();
    presenter.connected(generation);
    presenter
        .described(generation, &kumochi_scene::text::Bounded::default(), now)
        .unwrap();
    session.attach_narration(&mut presenter);

    let run = RunId(7);
    let authored = fixture::step(
        r#"{"step":{"id":"authored","start":"speech","say":"words","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
    );
    let begin = |scene| DataToShell::SceneBegin {
        scene,
        header: fixture::header(),
    };
    let commit = |scene| DataToShell::SceneStepCommit {
        run,
        scene,
        index: 0,
        step: authored.clone(),
        speech: None,
    };
    let send = |message: &DataToShell| {
        let encoded = frame::encode(message, Pipe::Data).unwrap();
        (&*link.data).write_all(&encoded).unwrap();
        u32::try_from(encoded.len()).unwrap()
    };
    let credit = |bytes| {
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
            Some(DataToBrain::Credit { bytes })
        );
    };
    let arrive = |message: DataToShell| {
        let expected = send(&message);
        let event = events.recv_timeout(PATIENCE).unwrap_or_else(|error| {
            panic!("the attended reader did not deliver {message:?}: {error}")
        });
        assert!(
            matches!(&event, LinkEvent::SceneData { generation: seen, frame, bytes }
                if (*seen, &**frame, *bytes) == (generation, &message, expected)),
            "the attended reader delivered {message:?}, got {event:?}"
        );
        event
    };
    let deliver = |presenter: &mut Presenter, event| {
        let LinkEvent::SceneData { frame, bytes, .. } = event else {
            unreachable!()
        };
        (
            presenter.data(generation, frame, bytes, now).unwrap(),
            bytes,
        )
    };
    let settle = |presenter: &mut Presenter, effects, bytes| {
        let effects = fixture::settle(presenter, effects, now).unwrap();
        assert_eq!(fixture::credit(&effects), bytes);
        assert!(fixture::controls(&effects).is_empty());
        session.grant(bytes);
        credit(bytes);
        for job in fixture::jobs(effects) {
            assert!(
                presenter
                    .completed(job.run(&mut fixture::Shaper), now)
                    .unwrap()
                    .is_empty()
            );
        }
    };
    let apply = |presenter: &mut Presenter, event| {
        let (effects, bytes) = deliver(presenter, event);
        settle(presenter, effects, bytes);
    };
    let activate = |presenter: &mut Presenter, scene| {
        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(presenter.advance(generation, now).unwrap().is_empty());
            if fixture::activated(presenter, scene) != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the scene activates after contention"
            );
            let ready = session
                .retry_narration()
                .expect("only bridge contention defers activation");
            ready();
            assert!(session.resumes_narration(&session));
        }
        assert_eq!(fixture::activated(presenter, scene), 1);
    };

    for id in 1..=257 {
        let scene = SceneId(id);
        apply(&mut presenter, arrive(begin(scene)));
        assert_eq!(fixture::scene_ids(&presenter), [scene]);
        let absent = arrive(commit(scene));
        if matches!(id, 2 | 3) {
            let (admitting, commit_bytes) = deliver(&mut presenter, absent);
            let boundary = if id == 2 {
                DataToShell::ScenesRemoved {
                    scenes: vec![scene],
                }
            } else {
                DataToShell::SceneCutApplied {
                    scene,
                    revision: 1,
                    cut: kumochi_scene::StopCut {
                        activated: 0,
                        revealed: kumochi_scene::text::Bounded::default(),
                    },
                }
            };
            let (held, removal_bytes) = deliver(&mut presenter, arrive(boundary));
            assert!(held.is_empty(), "the terminal boundary is behind admission");
            assert_eq!(fixture::credit(&admitting), 0);
            assert_eq!(fixture::scene_ids(&presenter), [scene]);
            settle(&mut presenter, admitting, commit_bytes + removal_bytes);
            assert!(fixture::scene_ids(&presenter).is_empty());
            continue;
        }
        let replace = || {
            arrive(DataToShell::ScenesReplaced {
                scenes: vec![scene],
                successor: SceneId(id + 1),
            })
        };
        let queued_replacement = (id == 1).then(replace);
        apply(&mut presenter, absent);
        activate(&mut presenter, scene);
        apply(&mut presenter, queued_replacement.unwrap_or_else(replace));
        assert!(fixture::scene_ids(&presenter).is_empty());
    }

    let scene = SceneId(258);
    apply(&mut presenter, arrive(begin(scene)));
    apply(&mut presenter, arrive(commit(scene)));
    activate(&mut presenter, scene);
    let segment = SegmentId(0);
    let step = StepRef { scene, index: 1 };
    apply(
        &mut presenter,
        arrive(DataToShell::SceneStepCommit {
            run,
            scene,
            index: step.index,
            step: fixture::step(r#"{"step":{"id":"promised","start":"speech","say":"next"}}"#),
            speech: Some(SpeechPromise {
                output_generation: output,
                segment,
            }),
        }),
    );
    credit(send(&DataToShell::SegmentQueued {
        run,
        segment,
        start: SegmentStart::Speech,
        output_generation: output,
        step: Some(step),
    }));
    let LinkEvent::NarrationReady(wake) = next(&events) else {
        panic!("the matching promise is ready")
    };
    assert!(session.accepts_narration(wake));
    assert_eq!(
        presenter
            .conductor_view(now)
            .map(|level| level.binding.assignment),
        Some(Assignment {
            session: generation,
            output,
            run,
            segment,
            step,
        })
    );
    assert!(session.open());

    send(&commit(scene));
    queued_pair_closed(link.link, &link.control, &link.data);
}

#[cfg(debug_assertions)]
#[test]
fn stopped_narrated_run_drains_only_its_valid_late_announcement() {
    use kumochi_protocol::message::{SceneId, SegmentId, SegmentStart, SpeechPromise, StepRef};
    use kumochi_scene::{Record, parse_record};

    let link = narrated_attended("queued-late-narrated");
    let events = patient(link.events.clone());
    let (session, output) = queued_named_run(&link, &events);
    let run = RunId(7);
    let scene = SceneId(1);
    let segment = SegmentId(0);
    let Record::Step(step) =
        parse_record(br#"{"step":{"id":"speech-step","say":"test"}}"#).unwrap()
    else {
        panic!("step");
    };
    frame::write(
        &mut &*link.data,
        &DataToShell::SceneStepCommit {
            run,
            scene,
            index: 0,
            step,
            speech: Some(SpeechPromise {
                output_generation: output,
                segment,
            }),
        },
        Pipe::Data,
    )
    .unwrap();
    assert!(matches!(next(&events), LinkEvent::SceneData { .. }));
    assert!(link.handle.output().stop(session.generation(), run).is_ok());
    let mut announcement = DataToShell::SegmentQueued {
        run,
        segment,
        step: Some(StepRef { scene, index: 0 }),
        start: SegmentStart::Speech,
        output_generation: output,
    };
    frame::write(&mut &*link.data, &announcement, Pipe::Data).unwrap();
    let marker = DataToShell::AssistantText {
        run,
        delta: "after".into(),
    };
    let _sent = frame::write(&mut &*link.data, &marker, Pipe::Data);
    assert!(
        matches!(
            events.recv_timeout(PATIENCE),
            Ok(LinkEvent::Data { message, .. }) if message == marker
        ),
        "valid named-run metadata after Stop keeps the data reader live"
    );
    assert!(session.open());
    let DataToShell::SegmentQueued { step, .. } = &mut announcement else {
        unreachable!()
    };
    *step = Some(StepRef { scene, index: 1 });
    frame::write(&mut &*link.data, &announcement, Pipe::Data).unwrap();
    link.data.set_timeout(Some(PATIENCE));
    assert!(link.link.join().unwrap());
    assert!(!session.open(), "unrelated metadata still closes the pair");
}

#[cfg(debug_assertions)]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the two live pairs and the replacement in one ordered fixture"
)]
fn narrated_pump_rejects_foreign_equal_generations_without_sending_or_aborting() {
    use crate::speaker::{conductor::Assignment, queued::DrawingIo as _};
    use kumochi_protocol::message::{OutputGeneration, SceneId, SegmentId, StepRef};
    let first = narrated_attended("narrated-own-pair");
    let second = narrated_attended("narrated-foreign-pair");
    let events_a = patient(first.events.clone());
    let events_b = patient(second.events.clone());
    let (a, _) = queued_named_run(&first, &events_a);
    let (b, _) = queued_named_run(&second, &events_b);
    let narration = Arc::clone(a.io.narrated.as_ref().unwrap());
    let shared = Arc::clone(&first.handle.shared);
    assert_eq!(a.generation(), b.generation());
    let now = Instant::now();
    let mut presenter = crate::presenter::fixture::presenter();
    presenter.connected(a.generation());
    narration.attach(&mut presenter);
    for (handle, session) in [
        (&first.handle, &b),
        (&second.handle, &a),
        (&second.handle, &b),
    ] {
        assert_eq!(
            narration.update(handle, session, &mut presenter, now),
            Err(()),
            "a matching generation counter is not the original pair"
        );
    }
    let old = Assignment {
        session: a.generation(),
        output: OutputGeneration(1),
        run: RunId(7),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(1),
            index: 0,
        },
    };
    let fresh = Assignment {
        run: RunId(8),
        ..old
    };
    assert!(narration.promised(old));
    assert!(narration.announce(old));
    narration.run_end(old.run);
    let LinkEvent::NarrationReady(stale) = next(&events_a) else {
        panic!("the announcement and end share one wake");
    };
    narration.stop(old.run);
    assert!(narration.promised(fresh));
    assert!(narration.announce(fresh));
    let LinkEvent::NarrationReady(current) = next(&events_a) else {
        panic!("the new run has its own wake");
    };
    assert!(!a.accepts_narration(stale));
    assert!(a.accepts_narration(current));
    let mut old_worker = narration.worker(old.run, || {}, |_| {});
    assert_eq!(old_worker.view(), Ok(None));
    narration.run_end(fresh.run);
    let LinkEvent::NarrationReady(current) = next(&events_a) else {
        panic!("the run can wake again after its slot is taken");
    };
    assert!(
        !b.accepts_narration(current),
        "another pair cannot take an equal-generation wake"
    );
    assert!(matches!(
        events_a.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(old_worker);
    narration.run_end(fresh.run);
    let LinkEvent::NarrationReady(before_reconnect) = next(&events_a) else {
        panic!("leave a current-run wake in flight across reconnect");
    };
    for (link, session, events) in [(first, a, events_a), (second, b, events_b)] {
        assert!(session.open());
        let marker = DataToShell::AssistantText {
            run: RunId(7),
            delta: "alive".into(),
        };
        frame::write(&mut &*link.data, &marker, Pipe::Data).unwrap();
        assert!(matches!(next(&events), LinkEvent::Data { message, .. } if message == marker));
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).unwrap();
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
            Some(ControlToBrain::Pong),
            "no foreign control offer before Pong"
        );
        frame::write(
            &mut &link.control,
            &ControlToShell::SpeechRevoked { run: RunId(7) },
            Pipe::Control,
        )
        .unwrap();
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
            Some(ControlToBrain::SpeechStopped {
                run: RunId(7),
                cursor: None,
            }),
            "the worker proves quiet before the fixture replaces its pair"
        );
        link.handle.end(&session);
        assert!(link.link.join().unwrap());
    }
    let reconnected = attended_queued_shared("narrated-pair-reconnected", shared);
    let current = connected(reconnected.events.recv_blocking().unwrap());
    assert_eq!(current.generation(), old.session + 1);
    assert!(!Arc::ptr_eq(
        &narration,
        current.io.narrated.as_ref().unwrap()
    ));
    assert!(!current.accepts_narration(before_reconnect));
    reconnected.handle.end(&current);
    assert!(reconnected.link.join().unwrap());
}

#[cfg(debug_assertions)]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the same split-pipe fixture covers both run lifetimes and exact metadata verdicts"
)]
fn an_in_flight_old_promise_drains_only_once_after_the_next_named_run() {
    use crate::{presenter::fixture, runs::RunDisposition};
    use kumochi_protocol::message::{
        SceneId, SegmentId, SegmentStart, SpeechPcm, SpeechPromise, StepRef,
    };
    use kumochi_scene::{Record, parse_record};
    use std::cell::RefCell;
    use std::io::Write as _;

    #[derive(Clone, Copy, Debug)]
    enum Pause {
        Commit,
        Announcement,
        After,
        Lone,
        Empty,
        Wrong,
        Unpromised,
        Duplicate,
    }

    use Pause as At;
    let cases = [At::Commit, At::Announcement, At::After]
        .into_iter()
        .flat_map(|at| [(at, true), (at, false)])
        .chain(
            [
                At::Lone,
                At::Empty,
                At::Wrong,
                At::Unpromised,
                At::Duplicate,
            ]
            .map(|at| (at, true)),
        );
    for (at, stop_successor) in cases {
        let (device, opened) = std::sync::mpsc::sync_channel(1);
        let link = attended_queued_shared(
            &format!("late-queued-{at:?}-{stop_successor}"),
            Arc::new(Shared {
                narrated: true,
                fake_playback: Some(device),
                ..Shared::default()
            }),
        );
        let events = patient(link.events.clone());
        let (session, output) = queued_named_run(&link, &events);
        let narration = session.io.narrated.as_ref().unwrap();
        let (old, new) = (RunId(7), RunId(8));
        let scene = SceneId(2);
        let segment = SegmentId(0);
        let now = Instant::now();
        let presenter = RefCell::new(fixture::presenter());
        {
            let mut presenter = presenter.borrow_mut();
            presenter.connected(session.generation());
            presenter
                .described(
                    session.generation(),
                    &kumochi_scene::text::Bounded::default(),
                    now,
                )
                .unwrap();
            narration.attach(&mut presenter);
            presenter
                .data(
                    session.generation(),
                    kumochi_scene::deferred::Deferred::new(
                        DataToShell::SceneBegin {
                            scene,
                            header: fixture::header(),
                        },
                        0,
                    ),
                    0,
                    now,
                )
                .unwrap();
        }
        let Record::Step(authored) = parse_record(br#"{"step":{"id":"old","say":"old"}}"#).unwrap()
        else {
            panic!("old drawing step")
        };
        let speech = Some(SpeechPromise {
            output_generation: output,
            segment,
        });
        let commit_for = |run, scene| {
            frame::encode(
                &DataToShell::SceneStepCommit {
                    run,
                    scene,
                    index: 0,
                    step: authored.clone(),
                    speech,
                },
                Pipe::Data,
            )
            .unwrap()
        };
        let announce_for = |run, scene, index| {
            frame::encode(
                &DataToShell::SegmentQueued {
                    run,
                    segment,
                    start: SegmentStart::Speech,
                    output_generation: output,
                    step: Some(StepRef { scene, index }),
                },
                Pipe::Data,
            )
            .unwrap()
        };
        let commit = commit_for(old, scene);
        let announcement = announce_for(old, scene, u32::from(matches!(at, At::Wrong)));
        let send = |bytes: &[u8]| (&*link.data).write_all(bytes).unwrap();
        let credit = |len| {
            assert_eq!(
                frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
                Some(DataToBrain::Credit {
                    bytes: u32::try_from(len).unwrap()
                }),
                "exact credit for {at:?}, B stopped: {stop_successor}"
            );
        };
        let grant_commit = || {
            let event = next(&events);
            let LinkEvent::SceneData { frame, bytes, .. } = event else {
                panic!(
                    "A's exact commit must reach admission after {at:?}, B stopped: {stop_successor}"
                );
            };
            assert!(
                matches!(&*frame, DataToShell::SceneStepCommit { run, scene: id, index: 0, speech: promised, .. }
                if (*run, *id, *promised) == (old, scene, speech))
            );
            assert_eq!(usize::try_from(bytes).unwrap(), commit.len());
            if matches!(at, At::Commit | At::Lone) {
                let mut presenter = presenter.borrow_mut();
                let effects = presenter
                    .data_with_disposition(
                        session.generation(),
                        frame,
                        bytes,
                        RunDisposition::Stopped,
                        now,
                    )
                    .unwrap();
                let effects = fixture::settle(&mut presenter, effects, now).unwrap();
                assert_eq!(
                    fixture::first_logged_step(&presenter, scene).id.as_str(),
                    "old"
                );
                assert_eq!(
                    fixture::controls(&effects),
                    [&ControlToBrain::StepFailed {
                        run: old,
                        scene,
                        index: 0,
                        reason: kumochi_protocol::message::SceneFailure::Cancelled,
                        cut: fixture::zero_cut(),
                    }]
                );
                assert_eq!(fixture::activated(&presenter, scene), 0);
                assert_eq!(fixture::painted(&presenter.frames().borrow(), scene), None);
                assert_eq!(fixture::credit(&effects), bytes);
                session.grant(fixture::credit(&effects));
            } else {
                session.grant(bytes);
            }
            credit(commit.len());
        };
        let stop = |run| {
            let stop = ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            };
            assert!(link.handle.send_control(&session, stop.clone()));
            assert_eq!(link.handle.output().stop(session.generation(), run), Ok(()));
            assert_eq!(
                frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
                Some(stop)
            );
            assert_eq!(
                frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
                Some(ControlToBrain::SpeechStopped { run, cursor: None }),
                "the actual worker proves quiet before releasing the data suffix"
            );
        };
        let split = |bytes: &[u8]| bytes.len() - 1;
        match at {
            At::Commit | At::Lone => send(&commit[..split(&commit)]),
            At::Empty | At::Unpromised => {}
            _ => {
                send(&commit);
                grant_commit();
            }
        }
        match at {
            At::Announcement | At::Wrong | At::Unpromised | At::Duplicate => {
                send(&announcement[..split(&announcement)]);
            }
            At::After => {
                send(&announcement);
                credit(announcement.len());
            }
            _ => {}
        }
        stop(old);
        link.handle.output().with_locked(|| {});
        let submit = queued_text(2, "new owner");
        assert!(link.handle.send_data(&session, submit.clone()));
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
            Some(submit)
        );
        frame::write(
            &mut &link.control,
            &run_state(
                new.0,
                ThreadId(1),
                Phase::Thinking,
                Some(RunOrigin::Typed { turn: 2 }),
            ),
            Pipe::Control,
        )
        .unwrap();
        let mut event = next(&events);
        while let LinkEvent::NarrationReady(wake) = event {
            assert!(
                !session.accepts_narration(wake),
                "A was stopped before B was named"
            );
            event = next(&events);
        }
        assert!(matches!(event, LinkEvent::RunState { run, .. } if run == new));
        if stop_successor {
            stop(new);
        }

        match at {
            At::Commit | At::Lone => {
                send(&commit[split(&commit)..]);
                grant_commit();
                if matches!(at, At::Commit) {
                    send(&announcement);
                    credit(announcement.len());
                }
            }
            At::Announcement | At::Wrong | At::Unpromised | At::Duplicate => {
                send(&announcement[split(&announcement)..]);
                if matches!(at, At::Wrong | At::Unpromised) {
                    queued_pair_closed(link.link, &link.control, &link.data);
                    continue;
                }
                credit(announcement.len());
                if matches!(at, At::Duplicate) {
                    send(&announcement);
                    queued_pair_closed(link.link, &link.control, &link.data);
                    continue;
                }
            }
            At::After | At::Empty => {}
        }
        if matches!(at, At::Lone) {
            let new_scene = SceneId(3);
            let commit = commit_for(new, new_scene);
            send(&commit);
            let LinkEvent::SceneData { frame, bytes, .. } = next(&events) else {
                panic!("B's own stopped commit is independent of A's outstanding promise");
            };
            assert!(
                matches!(&*frame, DataToShell::SceneStepCommit { run, scene, .. }
                if (*run, *scene) == (new, new_scene))
            );
            session.grant(bytes);
            credit(commit.len());
            let announcement = announce_for(new, new_scene, 0);
            send(&announcement);
            credit(announcement.len());
        }
        if matches!(at, At::Commit | At::Announcement | At::After) {
            let pcm = frame::encode(
                &DataToShell::SpeechPcm(SpeechPcm {
                    run: old,
                    segment,
                    output_generation: output,
                    seq: 0,
                    samples: vec![125; 240],
                    last: Some(240),
                }),
                Pipe::Data,
            )
            .unwrap();
            send(&pcm);
            credit(pcm.len());
            assert!(matches!(
                opened.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
        }
        assert_eq!(
            narration.update(&link.handle, &session, &mut presenter.borrow_mut(), now),
            Ok(())
        );
        let marker = DataToShell::AssistantText {
            run: new,
            delta: "new data".into(),
        };
        frame::write(&mut &*link.data, &marker, Pipe::Data).unwrap();
        assert!(matches!(next(&events), LinkEvent::Data { message, .. } if message == marker));
        frame::write(&mut &link.control, &ControlToShell::Ping, Pipe::Control).unwrap();
        assert_eq!(
            frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).unwrap(),
            Some(ControlToBrain::Pong)
        );
        assert!(session.open());
        if !stop_successor {
            stop(new);
        }
        let end = |id| {
            frame::write(
                &mut &*link.data,
                &DataToShell::RunEnd { run: id },
                Pipe::Data,
            )
            .unwrap();
            assert!(
                matches!(next(&events), LinkEvent::Data { message: DataToShell::RunEnd { run }, .. } if run == id)
            );
        };
        end(new);
        let draft = queued_text(3, "preserved draft");
        assert!(
            !link.handle.admits_text(Some(&session)),
            "B's RunEnd and quiet do not settle A"
        );
        assert!(!link.handle.send_data(&session, draft.clone()));
        end(old);
        assert!(link.handle.admits_text(Some(&session)));
        assert!(link.handle.send_data(&session, draft.clone()));
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).unwrap(),
            Some(draft)
        );
        send(if matches!(at, At::Empty | At::Lone) {
            &commit
        } else {
            &announcement
        });
        queued_pair_closed(link.link, &link.control, &link.data);
    }
}

fn queued_pair_closed(link: thread::JoinHandle<bool>, control: &PipeClient, data: &PipeClient) {
    let (done, joined) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _finished = done.send(link.join().expect("the attended link didn't panic"));
    });
    assert_eq!(
        joined.recv_timeout(PATIENCE),
        Ok(true),
        "the link completed pair teardown without UI help"
    );
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &*control, Pipe::Control)
            .expect("the peer reads control EOF"),
        None,
        "the original control pipe was closed"
    );
    assert_eq!(
        frame::read::<DataToBrain>(&mut &*data, Pipe::Data).expect("the peer reads data EOF"),
        None,
        "the original data pipe was closed"
    );
}

#[cfg(debug_assertions)]
#[test]
fn a_lost_route_that_never_calls_back_goes_quiet_and_admits_the_next_turn() {
    use kumochi_protocol::message::ScreenChoice;

    let _device = serialize_playback();
    let (id, hello, probe, brain) =
        kumochi_brain::ipc::offline_plain_peer("0 text A reply cut off by an unplug.".into(), 64);
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let pipe = authenticated_primed(id, hello);
    let events = patient(pipe.events);
    let session = connected(next(&events));
    let admitted = || {
        let until = Instant::now() + PATIENCE;
        while !pipe.handle.admits_text(Some(&session)) {
            assert!(
                Instant::now() < until && session.open(),
                "the pair admits a turn"
            );
            thread::sleep(Duration::from_millis(2));
        }
    };
    let typed = |turn| {
        pipe.handle.send_data(
            &session,
            DataToBrain::TextSubmit {
                turn,
                thread: None,
                screen: ScreenChoice::None,
                text: "unplug".into(),
            },
        )
    };
    admitted();
    assert!(typed(1));
    let mut playback = pipe
        .playback
        .recv_timeout(PATIENCE)
        .expect("the reply opens the fake route");
    let mut samples = [0.0; 48];
    let until = Instant::now() + PATIENCE;
    while samples.iter().all(|sample| *sample == 0.0) {
        assert!(Instant::now() < until, "the reply starts playing");
        playback.render(&mut samples);
        thread::sleep(Duration::from_millis(2));
    }
    // Unplugged: the device reports loss and never calls back again.
    playback.lose();
    let until = Instant::now() + PATIENCE;
    while probe.quiet() == RunId(0) {
        assert!(
            Instant::now() < until && session.open(),
            "the destroyed route still reports quiet"
        );
        thread::sleep(Duration::from_millis(2));
    }
    admitted();
    assert!(typed(2), "the next typed turn is accepted");
    let until = Instant::now() + PATIENCE;
    while !matches!(
        events.recv_timeout(PATIENCE),
        Ok(LinkEvent::RunState {
            origin: Some(RunOrigin::Typed { turn: 2 }),
            ..
        })
    ) {
        assert!(Instant::now() < until, "the second turn runs");
    }
    drop(playback);
    pipe.handle.end(&session);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
}
