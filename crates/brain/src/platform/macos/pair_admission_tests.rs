//! What parent-bound admission writes on the inherited pair, read from the shell's ends.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    CAPABILITY_LEN, ControlToBrain, ControlToShell, DataToBrain, Hello, Incarnation, IssueSession,
};
use kumochi_protocol::{BUILD_ID, Secret};
use kumochi_session::{Channel, ShellAwaitsReply};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::Instant;

use super::super::stream::{RecordReader, RecordWriter};
use super::legs::PairLegs;
use super::tests::{SECRET, SHORT, bounded, brain, by, ends, hello, shell};
use super::{AuthenticatedPair, respond};
use crate::ipc::{self, Legs as _, SessionError, greet};
use crate::platform::ShellProcess;
use crate::runtime::IssueEpochs;
use crate::tap::Tap;

#[tokio::test]
async fn ready_is_one_control_byte_and_the_responder_speaks_only_after_the_shells_first_message() {
    let (pair, [mut control, data]) = ends();
    let shell = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    pair.ready(&shell, by()).await.expect("READY written");
    let mut first = [0; 1];
    bounded(control.read_exact(&mut first))
        .await
        .expect("READY read");
    assert_eq!(first, [0x52]);
    let mut responding = Box::pin(respond(pair, brain(), by()));
    assert!(
        tokio::time::timeout(SHORT, &mut responding).await.is_err(),
        "the responder waits for message 1"
    );
    let mut more = [0; 1];
    for end in [&control, &data] {
        assert_eq!(
            end.try_read(&mut more).map_err(|error| error.kind()),
            Err(io::ErrorKind::WouldBlock),
            "a running responder writes nothing before the shell's first message"
        );
    }
    let (_shell, init) = ShellAwaitsReply::start(&SECRET, Channel::Dev).expect("shell starts");
    let envelope = bounded(async {
        tokio::select! {
            refused = responding => panic!("the responder ended early: {:?}", refused.err()),
            envelope = async {
                control.write_all(&init.bytes).await.expect("message 1");
                let mut envelope = [0; 2];
                control.read_exact(&mut envelope).await.expect("message 2's envelope");
                envelope
            } => envelope,
        }
    })
    .await;
    assert_eq!(envelope, [0x00, 0x30]);
}

/// The shell's side of a bound pair: a frame writer per leg and the control leg's reader.
struct Peer {
    control: RecordWriter<OwnedWriteHalf>,
    data: RecordWriter<OwnedWriteHalf>,
    control_in: RecordReader<OwnedReadHalf>,
    _data_in: OwnedReadHalf,
}

impl Peer {
    async fn send(&mut self, (pipe, bytes): &(Pipe, Vec<u8>)) {
        let writer = match pipe {
            Pipe::Control => &mut self.control,
            Pipe::Data => &mut self.data,
        };
        writer.write_frame(bytes).await.expect("written");
    }

    /// Every control frame the brain wrote before it closed the pair.
    async fn received(&mut self) -> Vec<ControlToShell> {
        let mut frames = Vec::new();
        while let Some(frame) = bounded(ipc::read(&mut self.control_in, Pipe::Control))
            .await
            .expect("a whole frame or the end")
        {
            frames.push(frame);
        }
        frames
    }
}

async fn bound() -> (AuthenticatedPair, Peer) {
    let (pair, [mut control, mut data]) = ends();
    let (pair, session) = bounded(async {
        tokio::join!(
            respond(pair, brain(), by()),
            shell(&mut control, &mut data, &SECRET),
        )
    })
    .await;
    let (control_in, control_out) = control.into_split();
    let (data_in, data_out) = data.into_split();
    let (cell, _) = watch::channel(false);
    let peer = Peer {
        control: RecordWriter::new(
            control_out,
            session.control.sealer,
            Pipe::Control,
            cell.clone(),
        ),
        data: RecordWriter::new(data_out, session.data.sealer, Pipe::Data, cell.clone()),
        control_in: RecordReader::new(control_in, session.control.opener, cell),
        _data_in: data_in,
    };
    (pair.expect("bound"), peer)
}

const INCARNATION: Incarnation = Incarnation(9);

/// Greets over a fresh bound pair whose shell sends `frames` first, then closes the pair and
/// returns the outcome, the control frames the brain wrote, and how many sessions it reserved.
async fn greeting(
    shell: &ShellProcess,
    frames: &[(Pipe, Vec<u8>)],
) -> (Result<bool, SessionError>, Vec<ControlToShell>, u64) {
    greeting_reserving(shell, frames, || {}).await
}

/// [`greeting`], running `on_reserve` inside the reservation.
async fn greeting_reserving(
    shell: &ShellProcess,
    frames: &[(Pipe, Vec<u8>)],
    on_reserve: impl FnOnce(),
) -> (Result<bool, SessionError>, Vec<ControlToShell>, u64) {
    let (mut pair, mut peer) = Box::pin(bound()).await;
    for frame in frames {
        peer.send(frame).await;
    }
    let mut epochs = IssueEpochs::default();
    let mut reserved = 0;
    let by = by();
    let result = Box::pin(greet(
        &mut PairLegs::new(&mut pair, shell, by),
        &mut Tap::default(),
        by,
        || {
            reserved += 1;
            on_reserve();
            epochs.reserve(INCARNATION)
        },
    ))
    .await
    .map(|admitted| admitted.is_some());
    drop(pair);
    (result, peer.received().await, reserved)
}

fn control(message: &ControlToBrain) -> (Pipe, Vec<u8>) {
    (
        Pipe::Control,
        frame::encode(message, Pipe::Control).expect("encodes"),
    )
}

fn data(message: &DataToBrain) -> (Pipe, Vec<u8>) {
    (
        Pipe::Data,
        frame::encode(message, Pipe::Data).expect("encodes"),
    )
}

fn welcome() -> ControlToShell {
    ControlToShell::Welcome {
        incarnation: INCARNATION,
        issue_epoch: 1,
    }
}

#[tokio::test]
async fn the_shared_greeting_runs_over_the_pair_in_order_and_logs_nothing() {
    let captured = crate::test_support::Captured::default();
    let capturing = captured.install();
    let shell = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    let echo = |epoch| IssueSession {
        incarnation: INCARNATION,
        epoch,
    };
    let metrics = data(&DataToBrain::TextMetrics(None));
    let other_build = Hello {
        build_id: "0.0.0+other".to_owned(),
        capability: Secret::new([0; CAPABILITY_LEN]),
    };
    let mut planted = [0; CAPABILITY_LEN];
    planted[CAPABILITY_LEN - 1] = 1;
    let planted_other_build = Hello {
        build_id: "0.0.0+other".to_owned(),
        capability: Secret::new(planted),
    };

    let legacy = [
        control(&ControlToBrain::Hello(hello())),
        data(&DataToBrain::Hello(hello())),
        metrics.clone(),
    ];
    let (result, frames, reserved) = greeting(&shell, &legacy).await;
    assert_eq!(
        (result.expect("Legacy"), frames, reserved),
        (true, vec![welcome()], 1)
    );

    let queued = [
        control(&ControlToBrain::QueuedHello(hello())),
        data(&DataToBrain::QueuedHello(hello(), echo(1))),
        metrics.clone(),
    ];
    let (result, frames, reserved) = greeting(&shell, &queued).await;
    assert_eq!(
        (result.expect("Queued"), frames, reserved),
        (true, vec![welcome()], 1)
    );

    let stale_echo = [
        control(&ControlToBrain::QueuedHello(hello())),
        data(&DataToBrain::QueuedHello(hello(), echo(2))),
        metrics.clone(),
    ];
    let (result, frames, _) = greeting(&shell, &stale_echo).await;
    assert!(
        matches!(result, Err(SessionError::Protocol(_))),
        "{result:?}"
    );
    assert_eq!(frames, vec![welcome()]);

    let legacy_data_after_queued = [
        control(&ControlToBrain::QueuedHello(hello())),
        data(&DataToBrain::Hello(hello())),
        metrics.clone(),
    ];
    let (result, frames, _) = greeting(&shell, &legacy_data_after_queued).await;
    assert!(matches!(result, Err(SessionError::NotHello)), "{result:?}");
    assert_eq!(frames, vec![welcome()]);

    let placeholder_first = [control(&ControlToBrain::Hello(planted_other_build))];
    let (result, frames, reserved) = greeting(&shell, &placeholder_first).await;
    assert!(
        matches!(result, Err(SessionError::WrongCapability)),
        "{result:?}"
    );
    assert_eq!((frames, reserved), (Vec::new(), 0));

    let mismatch = [control(&ControlToBrain::Hello(other_build))];
    let (result, frames, reserved) = greeting(&shell, &mismatch).await;
    let told = ControlToShell::BuildMismatch {
        brain_build: BUILD_ID.to_owned(),
    };
    assert_eq!(
        (result.expect("told"), frames, reserved),
        (false, vec![told], 0)
    );

    let gone = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    assert!(gone.publish(1));
    let (result, frames, reserved) = greeting(&gone, &legacy).await;
    assert!(
        matches!(result, Err(SessionError::ClosedEarly)),
        "{result:?}"
    );
    assert_eq!((frames, reserved), (Vec::new(), 0));

    drop(capturing);
    assert_eq!(captured.text(), "");
}

#[tokio::test]
async fn a_shell_lost_before_a_greeting_reply_gets_neither_reply() {
    let other_build = Hello {
        build_id: "0.0.0+other".to_owned(),
        capability: Secret::new([0; CAPABILITY_LEN]),
    };
    let lost = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    assert!(lost.publish(1));
    let mismatch = [control(&ControlToBrain::Hello(other_build))];
    let (result, frames, reserved) = greeting(&lost, &mismatch).await;
    assert!(
        matches!(result, Err(SessionError::ClosedEarly)),
        "{result:?}"
    );
    assert_eq!((frames, reserved), (Vec::new(), 0));

    let losing = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    let legacy = [
        control(&ControlToBrain::Hello(hello())),
        data(&DataToBrain::Hello(hello())),
        data(&DataToBrain::TextMetrics(None)),
    ];
    let (result, frames, reserved) = greeting_reserving(&losing, &legacy, || {
        assert!(losing.publish(1), "the loss comes before any claim");
    })
    .await;
    assert!(
        matches!(result, Err(SessionError::ClosedEarly)),
        "{result:?}"
    );
    assert_eq!((frames, reserved), (Vec::new(), 1));
}

#[tokio::test]
async fn the_pair_refuses_a_reply_once_its_deadline_has_passed() {
    let shell = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    let (mut pair, _peer) = Box::pin(bound()).await;
    let legs = PairLegs::new(&mut pair, &shell, by());
    assert!(matches!(legs.before_reply(by()), Ok(())));
    let passed = Instant::now();
    assert!(matches!(
        legs.before_reply(passed),
        Err(SessionError::Timeout)
    ));
}
