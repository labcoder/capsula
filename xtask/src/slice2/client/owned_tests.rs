use std::cell::Cell;
use std::io::{self, Write as _};
use std::time::{Duration, Instant};
use std::{sync::mpsc, thread};

use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    CAPABILITY_LEN, Capability, ControlToBrain, ControlToShell, DataToBrain, DataToShell,
    Incarnation, RunId,
};

use super::{Frame, PipeClient, credited_data, hello, owned_connect, read, welcome};
use crate::platform::gate_pipe::fixture_server;

fn name(label: &str) -> String {
    format!(
        r"\\.\pipe\kumochi-owned-fixture-{}-{label}",
        std::process::id()
    )
}

fn capability() -> Capability {
    Capability::new([3; CAPABILITY_LEN])
}

fn welcome_frame() -> ControlToShell {
    ControlToShell::Welcome {
        incarnation: Incarnation(42),
        issue_epoch: 7,
    }
}

#[test]
fn the_owned_server_opens_before_its_capability_is_published() {
    let ctl = name("startup-control");
    let data = name("startup-data");
    let mut control_server = fixture_server(&ctl);
    let mut data_server = fixture_server(&data);
    let mut reads = 0;
    let shared = owned_connect(
        &ctl,
        &data,
        std::process::id(),
        Instant::now() + Duration::from_secs(2),
        || {
            reads += 1;
            if reads < 3 {
                return Ok(None);
            }
            frame::write(&mut control_server, &welcome_frame(), Pipe::Control)?;
            Ok(Some(capability()))
        },
    )
    .expect("kept the owned pre-publication handle");
    assert_eq!(reads, 3);
    let (msg, _) = read::<DataToBrain>(&mut data_server, Pipe::Data)
        .expect("valid message")
        .expect("data Hello");
    let DataToBrain::Hello(seen) = msg else {
        panic!("not a Hello");
    };
    assert_eq!(seen.build_id, kumochi_protocol::BUILD_ID);
    assert_eq!(seen.capability.expose(), capability().expose());
    assert_eq!(
        shared.control.server_pid().expect("control owner"),
        std::process::id()
    );
}

#[test]
fn a_foreign_control_handle_is_refused_before_loading_the_capability() {
    let ctl = name("foreign-control");
    let _server = fixture_server(&ctl);
    let rejected = owned_connect(
        &ctl,
        &name("foreign-data"),
        std::process::id() ^ 1,
        Instant::now() + Duration::from_secs(2),
        || panic!("must not read a capability from a foreign handle"),
    );
    assert_eq!(
        rejected
            .expect_err("wrong PID")
            .downcast_ref::<io::Error>()
            .expect("native error")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn an_early_refusal_reconnects_only_after_the_capability_changes() {
    let ctl = name("stale-control");
    let data = name("stale-data");
    let mut first = fixture_server(&ctl);
    let mut data_server = fixture_server(&data);
    let (starting, accepted) = mpsc::sync_channel(1);
    let (replacement, ready) = mpsc::sync_channel(1);
    let mut starting = Some(starting);
    let mut live_second = None;
    let ctl_again = ctl.clone();
    thread::scope(|scope| {
        scope.spawn(move || {
            accepted
                .recv_timeout(Duration::from_secs(3))
                .expect("first control open");
            let (first_hello, _) = read::<ControlToBrain>(&mut first, Pipe::Control)
                .expect("old Hello frame")
                .expect("old Hello");
            let ControlToBrain::Hello(first_hello) = first_hello else {
                panic!("not a Hello");
            };
            assert_eq!(first_hello.capability.expose(), capability().expose());
            let new = fixture_server(&ctl_again);
            drop(first);
            replacement.send(new).expect("publish replacement");
        });
        let _shared = owned_connect(
            &ctl,
            &data,
            std::process::id(),
            Instant::now() + Duration::from_secs(3),
            || {
                if let Some(starting) = starting.take() {
                    starting.send(()).expect("first owned open");
                }
                if let Ok(mut second) = ready.try_recv() {
                    frame::write(&mut second, &welcome_frame(), Pipe::Control)?;
                    // Keep the second server alive until the data half has accepted its Hello.
                    live_second = Some(second);
                    return Ok(Some(Capability::new([4; CAPABILITY_LEN])));
                }
                Ok(Some(capability()))
            },
        )
        .expect("reopened with the changed capability");
        let (msg, _) = read::<DataToBrain>(&mut data_server, Pipe::Data)
            .expect("valid frame")
            .expect("data Hello");
        let DataToBrain::Hello(msg) = msg else {
            panic!("not a data Hello");
        };
        assert_eq!(msg.capability.expose(), &[4; CAPABILITY_LEN]);
        drop(live_second);
    });
}

#[test]
fn a_fragmented_welcome_uses_one_absolute_deadline() {
    let ctl = name("fragmented-welcome");
    let mut server = fixture_server(&ctl);
    let client = PipeClient::open(&ctl).expect("fixture connection");
    let mut wire = Vec::new();
    frame::write(&mut wire, &welcome_frame(), Pipe::Control).expect("valid Welcome");
    server.write_all(&wire[..HEADER_LEN]).expect("write header");
    let budget = Duration::from_millis(800);
    thread::scope(|scope| {
        let (ready, start) = mpsc::sync_channel(1);
        let writes = scope.spawn(move || {
            let deadline = Instant::now() + budget;
            ready.send(deadline).expect("publish the fixture deadline");
            let fragment_time = deadline
                .checked_sub(Duration::from_millis(400))
                .expect("fragment time is within the fixture budget");
            thread::sleep(fragment_time.saturating_duration_since(Instant::now()));
            server
                .write_all(&wire[HEADER_LEN..=HEADER_LEN])
                .expect("write fragment");
            let fragment_at = Instant::now();
            let tail_time = deadline + Duration::from_millis(200);
            thread::sleep(tail_time.saturating_duration_since(Instant::now()));
            let tail_started = Instant::now();
            server
                .write_all(&wire[HEADER_LEN + 1..])
                .expect("write tail");
            (fragment_at, tail_started, Instant::now(), server)
        });
        let deadline = start
            .recv_timeout(Duration::from_secs(5))
            .expect("fragment writer is ready");
        let client = client.until(deadline);
        let result = welcome(&client, &hello(capability()));
        let finished_at = Instant::now();
        let (fragment_at, tail_started, tail_written, _server) =
            writes.join().expect("fragmented Welcome");
        assert!(
            fragment_at < deadline,
            "payload fragment was not sent in time"
        );
        assert!(tail_started > deadline, "tail was sent too soon");
        assert!(
            finished_at >= deadline,
            "the client timed out before its deadline"
        );
        assert!(
            fragment_at < finished_at && finished_at < tail_started,
            "the client timed out between the two payload writes"
        );
        assert!(
            tail_written < fragment_at + budget,
            "tail was too late even for a reset deadline"
        );
        assert_eq!(
            result
                .expect_err("whole Welcome was late")
                .downcast_ref::<io::Error>()
                .expect("native timeout")
                .kind(),
            io::ErrorKind::TimedOut
        );
    });
}

#[test]
fn a_stalled_payload_fails_at_the_transport_deadline() {
    let ctl = name("stalled-payload");
    let mut server = fixture_server(&ctl);
    let client = PipeClient::open(&ctl).expect("fixture connection");
    let mut wire = Vec::new();
    frame::write(&mut wire, &welcome_frame(), Pipe::Control).expect("valid Welcome");
    server
        .write_all(&wire[..HEADER_LEN])
        .expect("write only the header");
    let client = client.until(Instant::now() + Duration::from_millis(100));
    thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(240));
            drop(server);
        });
        let result = read::<ControlToShell>(&client, Pipe::Control);
        assert_eq!(
            result
                .expect_err("payload never arrived")
                .downcast_ref::<io::Error>()
                .expect("native timeout")
                .kind(),
            io::ErrorKind::TimedOut
        );
    });
}

#[test]
fn legacy_stamp_follows_credit_but_projected_stamp_preserves_the_read() {
    let read_at = Instant::now();
    let data = || Ok(Some((DataToShell::RunEnd { run: RunId(2) }, 17)));
    let credited_at = Cell::new(read_at);
    let credit = |size| {
        assert_eq!(size, 17);
        thread::sleep(Duration::from_millis(2));
        credited_at.set(Instant::now());
        Ok(())
    };
    let (legacy, size) = credited_data(data(), None, credit);
    assert!(legacy.at >= credited_at.get());
    let (projected, _) = credited_data(data(), Some(read_at), credit);
    assert_eq!(size, 17);
    assert!(matches!(
        legacy.frame,
        Frame::Data(DataToShell::RunEnd { run: RunId(2) }, 17)
    ));
    assert_eq!(projected.at, read_at);
}
