use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_channel::Receiver;
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Deleted, ErrorKey, Incarnation,
    Phase, RunId, ThreadId, UserError,
};

use super::{Connection, Link, LinkEvent, LinkHandle, Session, Shared, start, welcome};
use crate::platform::{PipeClient, test_pair};
use crate::runs::{Refused, Runs, Shown};
use crate::threads::Threads;
use kumochi_protocol::message::{
    EventId, PageEvent, PageEventKind, PageOf, ThreadCursor, ThreadKind, ThreadRow,
};

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
        main_thread: ThreadId(9),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
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
        request: 2,
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
/// How long a test waits for anything before it fails.
const PATIENCE: Duration = Duration::from_secs(5);

fn pair(label: &str) -> (PipeClient, PipeClient) {
    let name = format!(
        r"\\.\pipe\{}-test-{}-{label}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    test_pair(&name).expect("a test pipe")
}

/// A connection the link attends on its own thread, and the brain's ends of its pipes.
struct Attended {
    handle: LinkHandle,
    events: Receiver<LinkEvent>,
    control: PipeClient,
    data: Arc<PipeClient>,
    /// Returns whether the UI was still there when the connection ended.
    link: JoinHandle<bool>,
}

fn attended(label: &str) -> Attended {
    let (control, shell_control) = pair(&format!("{label}-control"));
    let (data, shell_data) = pair(&format!("{label}-data"));
    control.set_timeout(Some(PATIENCE));
    data.set_timeout(Some(PATIENCE));
    let (sender, events) = async_channel::unbounded();
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
    };
    let (handle, start) = start(link, sender);
    let connection = Connection {
        control: Arc::new(shell_control),
        data: Arc::new(shell_data),
        incarnation: Incarnation(1),
    };
    let link = thread::spawn(move || start.link.attend(connection, &start.events));
    Attended {
        handle,
        events,
        control,
        data: Arc::new(data),
        link,
    }
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

/// The link's events with a timeout on each.
fn patient(events: Receiver<LinkEvent>) -> mpsc::Receiver<LinkEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = events.recv_blocking() {
            if sender.send(event).is_err() {
                return;
            }
        }
    });
    receiver
}

fn next(events: &mpsc::Receiver<LinkEvent>) -> LinkEvent {
    events.recv_timeout(PATIENCE).expect("an event in time")
}

fn connected(event: LinkEvent) -> Session {
    let LinkEvent::Connected(session) = event else {
        panic!("Connected first, got {event:?}");
    };
    session
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
    let _session = connected(next(&events));
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
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Failed,
            error: Some(error),
        } if error.key() == ErrorKey::SignInNeeded
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
        &run_state(2, ThreadId(3), Phase::Answering),
        Pipe::Control,
    )
    .expect("normal run state");
    assert!(matches!(
        next(&events),
        LinkEvent::RunState {
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Answering,
            error: None
        }
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
            draft: DraftId(9),
            version: StateVersion(1),
            state: ActionState::Draft,
            approval: None,
        },
        ControlToShell::IssueRefused {
            draft: DraftId(9),
            reason: IssueRefused::NotAllowedNow,
        },
        ControlToShell::Ping,
    ] {
        frame::write(&mut &link.control, &message, Pipe::Control).expect("written");
    }
    assert_eq!(
        frame::read::<ControlToBrain>(&mut &link.control, Pipe::Control).expect("read"),
        Some(ControlToBrain::Pong)
    );
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
    drop(link.control);
    assert!(link.link.join().expect("joined"));
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
fn the_brain_s_first_frames_with_the_account_view_keep_the_connection() {
    use kumochi_protocol::message::{AccountView, AssistantName, MaxFrameRate, SettingsView};
    let (control, shell_control) = pair("first-frames-control");
    let (_data, shell_data) = pair("first-frames-data");
    control.set_timeout(Some(PATIENCE));
    let view = SettingsView {
        assistant_name: AssistantName::new("Cloud").expect("valid"),
        user_name: None,
        onboarded: true,
        main_thread: ThreadId(9),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
    };
    for message in [
        ControlToShell::Welcome {
            incarnation: Incarnation(4),
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
    let incarnation = welcome(&shell_control).expect("the handshake takes Welcome");
    assert_eq!(incarnation, Incarnation(4));
    let (sender, events) = async_channel::unbounded();
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
    };
    let (_handle, start) = start(link, sender);
    let connection = Connection {
        control: Arc::new(shell_control),
        data: Arc::new(shell_data),
        incarnation,
    };
    let link = thread::spawn(move || start.link.attend(connection, &start.events));
    let events = patient(events);
    let session = connected(next(&events));
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
fn an_ended_connection_s_sends_and_credit_never_reach_the_next() {
    let (a_control, a_shell_control) = pair("generation-a-control");
    let (a_data, a_shell_data) = pair("generation-a-data");
    let (b_control, b_shell_control) = pair("generation-b-control");
    let (b_data, b_shell_data) = pair("generation-b-data");
    for pipe in [&a_control, &a_data, &b_control, &b_data] {
        pipe.set_timeout(Some(PATIENCE));
    }
    let (sender, events) = async_channel::unbounded();
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
    };
    let (handle, start) = start(link, sender);
    let link = thread::spawn(move || {
        [
            (a_shell_control, a_shell_data),
            (b_shell_control, b_shell_data),
        ]
        .into_iter()
        .all(|(control, data)| {
            let connection = Connection {
                control: Arc::new(control),
                data: Arc::new(data),
                incarnation: Incarnation(1),
            };
            start.link.attend(connection, &start.events)
        })
    });
    let events = patient(events);
    let a = connected(next(&events));
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
            run, thread, phase, ..
        } => runs.apply_state(run, thread, phase).credit,
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
        LinkEvent::TurnRefused => {
            runs.refused();
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
        run_state(1, u, Phase::Thinking),
        ControlToShell::ThreadDeleted {
            thread: t,
            outcome: Deleted::Hidden,
            last_run: None,
        },
        ControlToShell::TurnRefused {
            turn: 2,
            thread: Some(t),
        },
        run_state(1, u, Phase::Done),
    ];
    let mut credit = Vec::new();
    for answer in answers {
        frame::write(&mut &link.control, &answer, Pipe::Control).expect("written");
        credit.push(apply(&mut runs, handle, next(&events)));
        if matches!(answer, ControlToShell::ThreadDeleted { .. }) {
            assert!(runs.holds(t), "T waits on the turn sent before its delete");
            assert!(!runs.select_thread(Some(t)));
        }
    }
    assert_eq!(credit, [bytes, 0, 0, 0], "the held text's credit, once");
    assert!(!runs.holds(t));
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
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        shared: Arc::new(Shared::default()),
    };
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
                incarnation,
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
    assert!(!runs.holds(t));
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
    runs.submit("next", |turn| handle.send_data(&b, turn))
        .expect("queued");
    assert_eq!(
        frame::read::<DataToBrain>(&mut &b_data, Pipe::Data).expect("read"),
        Some(DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "next".into(),
        })
    );
    frame::write(
        &mut &b_control,
        &run_state(4, u, Phase::Answering),
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

fn run_state(run: u64, thread: ThreadId, phase: Phase) -> ControlToShell {
    ControlToShell::RunState {
        origin: (phase == Phase::Thinking)
            .then_some(kumochi_protocol::message::RunOrigin::Typed { turn: 1 }),
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
