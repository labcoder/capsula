use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_channel::Receiver;
use capsula_protocol::frame::{self, Pipe};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, Incarnation, Phase, RunId, ThreadId,
};

use super::{Connection, Link, LinkEvent, LinkHandle, Session, Shared, start};
use crate::platform::{PipeClient, test_pair};
use crate::runs::{Refused, Runs};
use crate::threads::Threads;
use capsula_protocol::message::{
    EventId, PageEvent, PageEventKind, PageOf, ThreadCursor, ThreadRow,
};

#[test]
fn page_frames_decode_on_their_session_and_earn_exact_credit_when_applied_or_dropped() {
    let link = attended("pages");
    let events = patient(link.events);
    let session = connected(next(&events));
    let mut threads = Threads::default();
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
        }],
        next: Some(ThreadCursor {
            id: ThreadId(3),
            updated_ms: 9,
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
        threads.apply(message);
        session.grant(bytes);
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
    let name = format!(r"\\.\pipe\capsula-test-{}-{label}", std::process::id());
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
    runs.connected();
    assert_eq!(
        runs.submit("typed", |turn| handle.send_data(&a, turn)),
        Err(Refused::Offline),
        "a turn through A's session never goes to B, so its draft stays"
    );
    assert!(!handle.send_control(&a, ControlToBrain::Pong));
    runs.disconnected();
    runs.connected();
    assert_eq!(
        runs.submit("typed", |turn| handle.send_data(&b, turn)),
        Ok(())
    );
    b.grant(7);
    assert_eq!(
        frame::read::<DataToBrain>(&mut &b_data, Pipe::Data).expect("read"),
        Some(DataToBrain::TextSubmit {
            thread: None,
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
/// `cargo test --release -p capsula-shell -- --ignored --nocapture measure_`. The shell side
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
