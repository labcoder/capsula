use super::*;

#[test]
fn a_missing_pipe_only_launches_again_after_a_build_mismatch() {
    let name = format!(
        r"\\.\pipe\{}-test-connect-{}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    let brain = std::env::current_exe().unwrap();

    let (server, client) = super::super::test_pair(&name).unwrap();
    assert!(matches!(open(&name, &brain, None), Err(LinkError::Busy)));
    drop((client, server));
    assert!(matches!(open(&name, &brain, None), Err(LinkError::NoBrain)));

    let launches = std::cell::Cell::new(0);
    let mut outcomes = [
        (0, LinkError::Busy),
        (0, LinkError::NoBrain),
        (1, LinkError::NoBrain),
        (1, LinkError::BuildMismatch),
        (1, LinkError::NoBrain),
        (2, LinkError::NotOurBrain),
    ]
    .into_iter();
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = connect_with(
        deadline,
        || false,
        |_| panic!("not quitting"),
        |attempt| {
            assert_eq!(attempt, Some(deadline));
            let (expected_count, error) = outcomes.next().expect("unexpected retry after refusal");
            assert_eq!(launches.get(), expected_count);
            Err(error)
        },
        || {
            launches.set(launches.get() + 1);
            Ok(())
        },
    );
    assert!(matches!(result, Err(LinkError::NotOurBrain)));
    assert_eq!(launches.get(), 2);
    assert!(outcomes.next().is_none());
}

fn probe_legs(name: &str) -> [(PipeClient, PipeClient); 2] {
    ["first", "probe"].map(|leg| {
        let name = format!(
            r"\\.\pipe\{}-test-probe-{name}-{}-{leg}",
            kumochi_protocol::constants::OBJECT_PREFIX,
            std::process::id()
        );
        let (server, client) = super::super::test_pair(&name).expect("a fresh test pipe pair");
        server.set_timeout(Some(HANDSHAKE_TIMEOUT));
        client.set_timeout(Some(HANDSHAKE_TIMEOUT));
        (server, client)
    })
}

fn hello() -> Hello {
    Hello {
        build_id: BUILD_ID.to_owned(),
        capability: kumochi_protocol::message::Capability::new(
            [0xa7; kumochi_protocol::message::CAPABILITY_LEN],
        ),
    }
}

fn probed(
    first: PipeClient,
    audio: AudioSession,
    reopen: impl FnOnce() -> Result<PipeClient, LinkError>,
) -> LinkError {
    handshake_or_probe(
        first,
        hello(),
        audio,
        None,
        Some(Instant::now() + HANDSHAKE_TIMEOUT),
        DrawingText {
            profile: None,
            font_family: None,
        },
        || panic!("the probe never opens a data leg"),
        reopen,
    )
    .expect_err("no brain here completes a handshake")
}

#[test]
fn a_brain_that_cannot_read_the_queued_hello_is_asked_for_its_build() {
    let [(first_server, first), (probe_server, probe)] = probe_legs("older");
    let older = thread::spawn(move || {
        let greeting = frame::read::<ControlToBrain>(&mut &first_server, Pipe::Control).unwrap();
        assert_eq!(greeting, Some(ControlToBrain::QueuedHello(hello())));
        drop(first_server);
        let probe = frame::read::<ControlToBrain>(&mut &probe_server, Pipe::Control).unwrap();
        assert_eq!(probe, Some(ControlToBrain::Hello(hello())));
        let reply = ControlToShell::BuildMismatch {
            brain_build: "older".to_owned(),
        };
        frame::write(&mut &probe_server, &reply, Pipe::Control).unwrap();
        probe_server
    });
    let result = probed(first, AudioSession::QueuedRequired, move || Ok(probe));
    assert!(matches!(result, LinkError::BuildMismatch));
    drop(older.join().unwrap());
}

#[test]
fn a_probe_welcomed_by_a_matching_brain_stays_a_refusal() {
    let [(first_server, first), (probe_server, probe)] = probe_legs("matching");
    let brain = thread::spawn(move || {
        let greeting = frame::read::<ControlToBrain>(&mut &first_server, Pipe::Control).unwrap();
        assert_eq!(greeting, Some(ControlToBrain::QueuedHello(hello())));
        drop(first_server);
        let probe = frame::read::<ControlToBrain>(&mut &probe_server, Pipe::Control).unwrap();
        assert_eq!(probe, Some(ControlToBrain::Hello(hello())));
        let welcome = ControlToShell::Welcome {
            incarnation: kumochi_protocol::message::Incarnation(1),
            issue_epoch: 1,
        };
        frame::write(&mut &probe_server, &welcome, Pipe::Control).unwrap();
        probe_server
    });
    let result = probed(first, AudioSession::QueuedRequired, move || Ok(probe));
    assert!(matches!(result, LinkError::Rejected));
    drop(brain.join().unwrap());
}

#[test]
fn an_unanswered_legacy_hello_is_not_probed() {
    let [(first_server, first), _] = probe_legs("legacy");
    let brain = thread::spawn(move || {
        let greeting = frame::read::<ControlToBrain>(&mut &first_server, Pipe::Control).unwrap();
        assert_eq!(greeting, Some(ControlToBrain::Hello(hello())));
    });
    let result = probed(first, AudioSession::Legacy, || {
        panic!("only a queued greeting is probed")
    });
    assert!(matches!(result, LinkError::Rejected));
    brain.join().unwrap();
}
