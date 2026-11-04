use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_channel::Receiver;
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Deleted, Incarnation, Phase, RunId,
    ThreadId,
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
        let LinkEvent::Data {
            generation,
            message,
            bytes,
        } = next(&events)
        else {
            panic!("typed data")
        };
        assert_eq!(generation, session.generation());
        assert_eq!(message, sent);
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
            thread: Some(ThreadId(9)),
            text: "home".to_owned()
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
        delta: delta.to_owned(),
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
        run: RunId(2),
        thread: ThreadId(3),
        phase: Phase::Answering,
    };
    frame::write(&mut &link.control, &state, Pipe::Control).expect("state");
    assert!(matches!(
        next(&events),
        LinkEvent::RunState {
            run: RunId(2),
            thread: ThreadId(3),
            phase: Phase::Answering
        }
    ));
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
        thread: None,
        text: "typed".to_owned(),
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
    let _session = connected(next(&events));
    let LinkEvent::Settings {
        view: shown,
        answer: false,
        ..
    } = next(&events)
    else {
        panic!("settings first");
    };
    assert_eq!(shown, view);
    let LinkEvent::Scenes(scenes) = next(&events) else {
        panic!("scenes after the account view");
    };
    assert!(scenes.is_empty());
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
            thread: Some(ThreadId(1)),
            text: "typed".to_owned(),
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
        LinkEvent::RunState { run, thread, phase } => runs.apply_state(run, thread, phase).credit,
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
    for (thread, typed) in [(u, "u"), (t, "t")] {
        assert_eq!(
            frame::read::<DataToBrain>(&mut &*link.data, Pipe::Data).expect("read"),
            Some(DataToBrain::TextSubmit {
                thread: Some(thread),
                text: typed.to_owned(),
            })
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
        ControlToShell::TurnRefused { thread: Some(t) },
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
        Some(DataToBrain::TextSubmit {
            thread: Some(ThreadId(1)),
            text: "next".to_owned(),
        }),
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
            thread: Some(ThreadId(1)),
            text: "next".to_owned(),
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
        })
    );
    drop(b_control);
    assert!(link.join().expect("joined"));
}

fn run_state(run: u64, thread: ThreadId, phase: Phase) -> ControlToShell {
    ControlToShell::RunState {
        run: RunId(run),
        thread,
        phase,
    }
}

/// Plays the brain's control side: answers each Stop with `Interrupted`.
fn answer_stops(server: &PipeClient) {
    while let Ok(Some(message)) = frame::read::<ControlToBrain>(&mut &*server, Pipe::Control) {
        if let ControlToBrain::Stop { run, .. } = message {
            let state = ControlToShell::RunState {
                run,
                thread: ThreadId(1),
                phase: Phase::Interrupted,
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
            delta,
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
