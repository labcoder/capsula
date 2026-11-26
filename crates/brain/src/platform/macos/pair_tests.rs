use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream as StdStream};
use std::time::Duration;

use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    CAPABILITY_LEN, ControlToBrain, DataToBrain, Hello, Incarnation, IssueSession,
};
use kumochi_protocol::{BUILD_ID, Secret};
use kumochi_session::{Channel, ENVELOPE_LEN, SECRET_LEN, Session, SessionError, ShellAwaitsReply};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::Instant;

use super::*;
use crate::ipc::{self, PeerAuth};
use crate::platform::macos::stream::{RecordReader, RecordWriter};

/// A synthetic group secret; no real key is ever used here.
pub(super) const SECRET: [u8; SECRET_LEN] = [0x5A; SECRET_LEN];
const BOUND: Duration = Duration::from_secs(10);
pub(super) const SHORT: Duration = Duration::from_millis(60);

fn no_sigpipe(fd: &impl AsFd) {
    let on: libc::c_int = 1;
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("an int fits");
    // SAFETY: `on` is valid for reads of `len` bytes and the fd is open for the call.
    let set = unsafe {
        libc::setsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const on).cast(),
            len,
        )
    };
    assert_eq!(set, 0, "SO_NOSIGPIPE set");
}

/// A connected stream pair as the shell makes it: SIGPIPE suppressed on both ends.
pub(super) fn sockets() -> (StdStream, StdStream) {
    let (a, b) = StdStream::pair().expect("socketpair");
    no_sigpipe(&a);
    no_sigpipe(&b);
    (a, b)
}

pub(super) fn fd(stream: StdStream) -> OwnedFd {
    OwnedFd::from(stream)
}

fn id(dev: i64, ino: u64) -> Identity {
    Identity { dev, ino }
}

#[test]
fn an_identity_counts_only_when_two_samples_agree_and_are_numbered() {
    assert_eq!(stable(id(1, 7), id(1, 7)), Ok(id(1, 7)));
    assert_eq!(stable(id(1, 0), id(1, 0)), Err(PairError::Unstable));
    assert_eq!(stable(id(1, 0), id(1, 7)), Err(PairError::Unstable));
    assert_eq!(stable(id(1, 7), id(1, 8)), Err(PairError::Unstable));
    assert_eq!(stable(id(1, 7), id(2, 7)), Err(PairError::Unstable));
}

#[tokio::test]
async fn adoption_takes_two_connected_streams_and_makes_them_close_on_exec_and_non_blocking() {
    let (control, _shell_control) = sockets();
    let (data, _shell_data) = sockets();
    for end in [&control, &data] {
        rustix::io::fcntl_setfd(end, rustix::io::FdFlags::empty()).expect("inheritable");
        end.set_nonblocking(false).expect("blocking");
    }
    let pair = adopt(fd(control), fd(data)).expect("adopted");
    for end in [&pair.control, &pair.data] {
        let flags = rustix::io::fcntl_getfd(end).expect("fd flags");
        assert!(
            flags.contains(rustix::io::FdFlags::CLOEXEC),
            "close-on-exec"
        );
        let status = rustix::fs::fcntl_getfl(end).expect("status flags");
        assert!(
            status.contains(rustix::fs::OFlags::NONBLOCK),
            "non-blocking"
        );
    }
}

#[tokio::test]
async fn adoption_refuses_anything_but_two_connected_unix_streams() {
    let (reader, _writer) = std::io::pipe().expect("pipe");
    let (datagram, _peer) = UnixDatagram::pair().expect("datagram pair");
    let scratch = crate::scratch::dir("pair-listener");
    let listener = UnixListener::bind(scratch.path().join("l")).expect("listener");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let stream = std::net::TcpStream::connect(tcp.local_addr().expect("address")).expect("connect");
    no_sigpipe(&stream);
    let refused: [(OwnedFd, &str); 4] = [
        (OwnedFd::from(reader), "a pipe"),
        (OwnedFd::from(datagram), "a datagram socket"),
        (OwnedFd::from(listener), "a listener"),
        (OwnedFd::from(stream), "a connected TCP stream"),
    ];
    for (odd, what) in refused {
        let (good, _shell) = sockets();
        assert_eq!(
            adopt(odd, fd(good)).map(|_| ()),
            Err(PairError::NotAStream),
            "{what}"
        );
    }
    let (good, _shell) = sockets();
    let (bare, _peer) = StdStream::pair().expect("socketpair");
    assert_eq!(
        adopt(fd(good), fd(bare)).map(|_| ()),
        Err(PairError::SigPipe)
    );
}

#[tokio::test]
async fn adoption_refuses_one_socket_passed_as_both_legs() {
    let (control, _shell) = sockets();
    let duplicate = control.try_clone().expect("dup");
    assert_eq!(
        adopt(fd(control), fd(duplicate)).map(|_| ()),
        Err(PairError::Aliased)
    );
}

#[tokio::test]
async fn a_write_after_the_shell_closes_fails_with_epipe_instead_of_a_signal() {
    let (control, shell_control) = sockets();
    let (data, _shell_data) = sockets();
    let pair = adopt(fd(control), fd(data)).expect("adopted");
    drop(shell_control);
    let error = std::io::Write::write_all(&mut &pair.control, b"x").expect_err("the peer is gone");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}

pub(super) async fn handshake_message(
    io: &mut UnixStream,
    len: impl FnOnce([u8; ENVELOPE_LEN]) -> Result<usize, SessionError>,
) -> Vec<u8> {
    let mut envelope = [0; ENVELOPE_LEN];
    io.read_exact(&mut envelope).await.expect("envelope");
    let mut message = vec![0; len(envelope).expect("length")];
    io.read_exact(&mut message).await.expect("message");
    message
}

/// The honest shell: the initiator's steps over its two ends.
pub(super) async fn shell(
    control: &mut UnixStream,
    data: &mut UnixStream,
    secret: &[u8; SECRET_LEN],
) -> Session {
    let (mut shell, init) = ShellAwaitsReply::start(secret, Channel::Dev).expect("shell starts");
    control.write_all(&init.bytes).await.expect("init");
    let reply = handshake_message(control, |envelope| shell.record_len(envelope)).await;
    let (shell, [finished, data_init]) = shell.receive(&reply).expect("reply");
    control.write_all(&finished.bytes).await.expect("Finished");
    data.write_all(&data_init.bytes).await.expect("data init");
    let reply = handshake_message(data, |envelope| shell.record_len(envelope)).await;
    let (shell, [finished, bind]) = shell.receive(&reply).expect("data reply");
    data.write_all(&finished.bytes)
        .await
        .expect("data Finished");
    control.write_all(&bind.bytes).await.expect("Bind");
    let ack = handshake_message(control, |envelope| shell.record_len(envelope)).await;
    shell.receive(&ack).expect("BindAck")
}

/// The shell's side once bound: a frame writer and a frame reader per leg.
struct Shell {
    control: RecordWriter<OwnedWriteHalf>,
    data: RecordWriter<OwnedWriteHalf>,
    control_in: RecordReader<OwnedReadHalf>,
    data_in: RecordReader<OwnedReadHalf>,
}

impl Shell {
    async fn send<T: serde::Serialize>(&mut self, pipe: Pipe, message: &T) -> usize {
        let bytes = frame::encode(message, pipe).expect("encodes");
        let writer = match pipe {
            Pipe::Control => &mut self.control,
            Pipe::Data => &mut self.data,
        };
        writer.write_frame(&bytes).await.expect("written");
        bytes.len() - HEADER_LEN
    }
}

/// The adopted pair and the shell's two ends, as tokio streams.
pub(super) fn ends() -> (RegisteredPair, [UnixStream; 2]) {
    let (control, shell_control) = sockets();
    let (data, shell_data) = sockets();
    let pair = adopt(fd(control), fd(data))
        .expect("adopted")
        .register()
        .expect("registered");
    let shell = [shell_control, shell_data].map(|end| {
        end.set_nonblocking(true).expect("non-blocking");
        UnixStream::from_std(end).expect("tokio")
    });
    (pair, shell)
}

pub(super) fn brain() -> BrainAwaitsInit {
    BrainAwaitsInit::new(&SECRET, Channel::Dev).expect("brain starts")
}

/// Runs a two-sided exchange to completion within the bound, so a side that stops answering
/// fails the test instead of hanging it.
pub(super) async fn bounded<F: Future>(exchange: F) -> F::Output {
    tokio::time::timeout(BOUND, exchange)
        .await
        .expect("the exchange finishes within the bound")
}

async fn bound() -> (AuthenticatedPair, Shell) {
    let (pair, [mut shell_control, mut shell_data]) = ends();
    let (pair, session) = bounded(async {
        tokio::join!(
            respond(pair, brain(), by()),
            shell(&mut shell_control, &mut shell_data, &SECRET),
        )
    })
    .await;
    let pair = pair.expect("bound");
    let (shell_control_in, shell_control_out) = shell_control.into_split();
    let (shell_data_in, shell_data_out) = shell_data.into_split();
    let (cell, _) = watch::channel(false);
    let shell = Shell {
        control: RecordWriter::new(
            shell_control_out,
            session.control.sealer,
            Pipe::Control,
            cell.clone(),
        ),
        data: RecordWriter::new(
            shell_data_out,
            session.data.sealer,
            Pipe::Data,
            cell.clone(),
        ),
        control_in: RecordReader::new(shell_control_in, session.control.opener, cell.clone()),
        data_in: RecordReader::new(shell_data_in, session.data.opener, cell),
    };
    (pair, shell)
}

pub(super) fn hello() -> Hello {
    Hello::session(BUILD_ID.to_owned())
}

pub(super) fn by() -> Instant {
    Instant::now() + BOUND
}

#[tokio::test]
async fn bound_greetings_come_through_the_pairs_own_openers_and_pass_the_check() {
    let (mut pair, mut shell) = bound().await;
    let sent = shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    let (bound, opening, len) = pair.control_greeting(by()).await.expect("control greeting");
    assert_eq!((opening, len), (Opening::Legacy, sent));
    assert_eq!(bound.into_greeting(), hello());
    let sent = shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    let (bound, echo, len) = pair.data_greeting(by()).await.expect("data greeting");
    assert_eq!((echo, len), (None, sent));
    assert_eq!(ipc::check_hello(PeerAuth::SessionBound(bound)), Ok(()));
}

#[tokio::test]
async fn queued_greetings_return_their_mode_and_echo_as_plain_data() {
    let (mut pair, mut shell) = bound().await;
    let previous = IssueSession {
        incarnation: Incarnation(7),
        epoch: 3,
    };
    let echo = IssueSession {
        incarnation: Incarnation(9),
        epoch: 4,
    };
    let reconnect = ControlToBrain::QueuedAudioReconnect(Box::new(hello()), previous);
    shell.send(Pipe::Control, &reconnect).await;
    let (_, opening, _) = pair.control_greeting(by()).await.expect("control greeting");
    assert_eq!(opening, Opening::Reconnect(previous));
    shell
        .send(Pipe::Data, &DataToBrain::QueuedHello(hello(), echo))
        .await;
    let (_, got, _) = pair.data_greeting(by()).await.expect("data greeting");
    assert_eq!(got, Some(echo));
}

#[tokio::test]
async fn a_data_greeting_in_the_other_mode_is_refused() {
    let echo = IssueSession {
        incarnation: Incarnation(1),
        epoch: 1,
    };
    let cases = [
        (
            ControlToBrain::Hello(hello()),
            DataToBrain::QueuedHello(hello(), echo),
        ),
        (
            ControlToBrain::QueuedHello(hello()),
            DataToBrain::Hello(hello()),
        ),
    ];
    for (control, data) in cases {
        let (mut pair, mut shell) = bound().await;
        shell.send(Pipe::Control, &control).await;
        pair.control_greeting(by()).await.expect("control greeting");
        shell.send(Pipe::Data, &data).await;
        assert!(
            matches!(pair.data_greeting(by()).await, Err(PairError::Greeting)),
            "{control:?}"
        );
        assert!(*pair.pair.borrow(), "the pair is refused");
    }
}

#[tokio::test]
async fn each_greeting_is_read_once_and_control_first() {
    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    assert!(matches!(
        pair.data_greeting(by()).await,
        Err(PairError::Order)
    ));
    assert!(
        matches!(pair.control_greeting(by()).await, Err(PairError::Order)),
        "a refusal stays"
    );

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    pair.control_greeting(by()).await.expect("control greeting");
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    assert!(matches!(
        pair.control_greeting(by()).await,
        Err(PairError::Order)
    ));
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    assert!(
        matches!(pair.data_greeting(by()).await, Err(PairError::Order)),
        "a refusal stays"
    );

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    pair.control_greeting(by()).await.expect("control greeting");
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    pair.data_greeting(by()).await.expect("data greeting");
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    assert!(matches!(
        pair.data_greeting(by()).await,
        Err(PairError::Order)
    ));
}

#[tokio::test]
async fn a_first_frame_that_isnt_a_greeting_refuses_the_whole_pair() {
    let (mut pair, mut shell) = bound().await;
    shell.send(Pipe::Control, &ControlToBrain::Pong).await;
    assert!(matches!(
        pair.control_greeting(by()).await,
        Err(PairError::Greeting)
    ));
    assert!(*pair.pair.borrow(), "the pair cell is set");
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    assert!(matches!(
        pair.data_greeting(by()).await,
        Err(PairError::Order)
    ));
}

#[tokio::test]
async fn a_bound_greeting_with_a_nonzero_placeholder_is_refused() {
    let (mut pair, mut shell) = bound().await;
    let mut planted = [0; CAPABILITY_LEN];
    planted[CAPABILITY_LEN - 1] = 1;
    let greeting = Hello {
        build_id: BUILD_ID.to_owned(),
        capability: Secret::new(planted),
    };
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(greeting))
        .await;
    let (bound, _, _) = pair.control_greeting(by()).await.expect("control greeting");
    assert_eq!(
        ipc::check_hello(PeerAuth::SessionBound(bound)),
        Err(ipc::Refusal::WrongCapability)
    );
}

#[tokio::test]
async fn a_greeting_that_misses_its_deadline_refuses_the_pair() {
    let (mut pair, _shell) = bound().await;
    let by = Instant::now() + SHORT;
    let refused = tokio::time::timeout(BOUND, pair.control_greeting(by))
        .await
        .expect("the read ends within the bound");
    assert!(matches!(refused, Err(PairError::Timeout)));
    assert!(Instant::now() >= by, "waited for the deadline");
    assert!(*pair.pair.borrow(), "the pair cell is set");
}

/// One completed handshake: the brain's owners and proof, its two ends and the shell's.
async fn run() -> (Session, BoundPair, [UnixStream; 2], [UnixStream; 2]) {
    let (pair, [mut shell_control, mut shell_data]) = ends();
    let RegisteredPair {
        mut control,
        mut data,
    } = pair;
    let (run, _) = bounded(async {
        tokio::join!(
            handshake(&mut control, &mut data, brain(), by()),
            shell(&mut shell_control, &mut shell_data, &SECRET),
        )
    })
    .await;
    let (session, proof) = run.expect("handshake");
    (session, proof, [control, data], [shell_control, shell_data])
}

#[tokio::test]
async fn a_proof_names_both_of_its_own_openers_and_no_others() {
    let (own, proof, [control, data], _own_shell) = run().await;
    let (other, _, _, _other_shell) = run().await;
    let foreign_data = Session {
        control: own.control,
        data: other.data,
    };
    assert!(matches!(
        AuthenticatedPair::new(foreign_data, &proof, control, data),
        Err(PairError::Unbound)
    ));
    let (own, proof, [control, data], _own_shell) = run().await;
    let (other, _, _, _other_shell) = run().await;
    let foreign_control = Session {
        control: other.control,
        data: own.data,
    };
    assert!(matches!(
        AuthenticatedPair::new(foreign_control, &proof, control, data),
        Err(PairError::Unbound)
    ));
}

#[test]
fn adoption_needs_no_runtime() {
    let (control, _shell_control) = sockets();
    let (data, _shell_data) = sockets();
    assert!(adopt(fd(control), fd(data)).is_ok());
}

#[tokio::test]
async fn the_handshake_ends_at_its_deadline() {
    let (pair, _shell) = ends();
    let by = Instant::now() + SHORT;
    let refused = tokio::time::timeout(BOUND, respond(pair, brain(), by))
        .await
        .expect("the handshake ends within the bound");
    assert!(matches!(refused, Err(PairError::Timeout)));
    assert!(Instant::now() >= by, "waited for the deadline");
}

#[tokio::test]
async fn a_shell_with_another_secret_is_refused() {
    let (pair, [mut shell_control, _shell_data]) = ends();
    let (init, _) = ShellAwaitsReply::start(&[0xA5; SECRET_LEN], Channel::Dev)
        .map(|(state, init)| (init, state))
        .expect("shell starts");
    shell_control.write_all(&init.bytes).await.expect("init");
    assert!(matches!(
        respond(pair, brain(), by()).await,
        Err(PairError::Session(SessionError::Handshake))
    ));
}

#[tokio::test]
async fn ready_input_after_the_deadline_still_misses_it() {
    let (pair, [mut shell_control, _shell_data]) = ends();
    let (_, init) = ShellAwaitsReply::start(&SECRET, Channel::Dev).expect("shell starts");
    shell_control.write_all(&init.bytes).await.expect("init");
    let refused = respond(pair, brain(), Instant::now()).await;
    assert!(matches!(refused, Err(PairError::Timeout)));

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    let refused = pair.control_greeting(Instant::now()).await;
    assert!(matches!(refused, Err(PairError::Timeout)));
    assert!(*pair.pair.borrow(), "the pair cell is set");
}

#[tokio::test]
async fn a_greeting_read_dropped_part_way_refuses_the_pair() {
    let (mut pair, _shell) = bound().await;
    let dropped = tokio::time::timeout(SHORT, pair.control_greeting(by())).await;
    assert!(
        dropped.is_err(),
        "nothing was sent, so the read was still pending"
    );
    assert!(*pair.pair.borrow(), "the pair cell is set before any retry");
}

#[tokio::test]
async fn the_legs_are_handed_over_only_after_both_greetings() {
    let (mut pair, _shell) = bound().await;
    assert_eq!(pair.legs().map(|_| ()), Err(PairError::Order));

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    pair.control_greeting(by()).await.expect("control greeting");
    assert_eq!(pair.legs().map(|_| ()), Err(PairError::Order));

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    pair.control_greeting(by()).await.expect("control greeting");
    pair.data_greeting(by()).await.expect("data greeting");
    assert!(pair.legs().is_ok());
}

/// The runtime over a greeted pair's own legs: a run's text crosses multi-record frames inside the
/// data window without credit, pings are answered and Quitting ends it. The shell's end here uses
/// the brain's tokio adapters, not the shell's blocking adapter.
#[tokio::test]
async fn the_runtime_serves_a_greeted_pair_over_its_own_legs() {
    use kumochi_protocol::message::{
        AudioSession, ControlToShell, DataToShell, Phase, RunId, ScreenChoice,
    };

    use crate::runtime::{Ended, PairStart};

    let scratch = crate::test_support::Scratch::new("pair-serve");
    let mut brain = crate::test_support::brain(&scratch, "0 fill 240000\n0 fill 240000");
    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    pair.control_greeting(by()).await.expect("control greeting");
    let welcome = ControlToShell::Welcome {
        incarnation: Incarnation(1),
        issue_epoch: 1,
    };
    let bytes = frame::encode(&welcome, Pipe::Control).expect("encodes");
    pair.write_control(&bytes).await.expect("Welcome out");
    let got = bounded(ipc::read::<ControlToShell>(
        &mut shell.control_in,
        Pipe::Control,
    ))
    .await;
    assert_eq!(got.expect("Welcome in"), Some(welcome));
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    pair.data_greeting(by()).await.expect("data greeting");
    let (_parent, loss) = watch::channel(Verdict::Watching);
    let terminal = Terminal::new(pair.pair.subscribe(), Some(loss));
    let (control, data) = pair.legs().expect("greeted");
    let mut tap = crate::tap::Tap::from_env();
    let client = async {
        shell
            .send(
                Pipe::Data,
                &DataToBrain::TextSubmit {
                    turn: 1,
                    thread: None,
                    screen: ScreenChoice::None,
                    text: "go".into(),
                },
            )
            .await;
        let mut text = 0;
        while text < 480_000 {
            match ipc::read::<DataToShell>(&mut shell.data_in, Pipe::Data).await {
                Ok(Some(DataToShell::AssistantText { delta, .. })) => text += delta.len(),
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
        assert_eq!(text, 480_000);
        loop {
            match ipc::read::<ControlToShell>(&mut shell.control_in, Pipe::Control).await {
                Ok(Some(ControlToShell::Ping)) => {
                    shell.send(Pipe::Control, &ControlToBrain::Pong).await;
                }
                Ok(Some(ControlToShell::RunState {
                    run: RunId(1),
                    phase: Phase::Done,
                    ..
                })) => break,
                Ok(Some(_)) => {}
                other => panic!("the control leg ended: {other:?}"),
            }
        }
        shell.send(Pipe::Control, &ControlToBrain::Quitting).await;
    };
    let (ended, ()) = bounded(async {
        tokio::join!(
            Box::pin(brain.serve_split(
                control,
                data,
                &mut tap,
                AudioSession::Legacy,
                None,
                PairStart::Ordinary,
                Some(terminal),
            )),
            client,
        )
    })
    .await;
    assert!(matches!(ended, Ended::Quit { .. }), "{ended:?}");
    brain.close().await;
}

#[tokio::test]
async fn a_served_pair_ends_as_fatal_once_its_parent_exits() {
    use kumochi_protocol::message::AudioSession;

    use crate::runtime::{Ended, Fatal, PairStart};

    let scratch = crate::test_support::Scratch::new("pair-parent");
    let mut brain = crate::test_support::brain(&scratch, "");
    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
    pair.control_greeting(by()).await.expect("control greeting");
    pair.data_greeting(by()).await.expect("data greeting");
    let (parent, loss) = watch::channel(Verdict::Watching);
    let terminal = Terminal::new(pair.pair.subscribe(), Some(loss));
    let (control, data) = pair.legs().expect("greeted");
    let mut tap = crate::tap::Tap::from_env();
    let (ended, ()) = bounded(async {
        tokio::join!(
            Box::pin(brain.serve_split(
                control,
                data,
                &mut tap,
                AudioSession::Legacy,
                None,
                PairStart::Ordinary,
                Some(terminal),
            )),
            async {
                tokio::time::sleep(SHORT).await;
                parent.send_replace(Verdict::Exited);
            },
        )
    })
    .await;
    assert_eq!(ended, Ended::Fatal(Fatal::ParentExited));
    drop(shell);
    brain.close().await;
}

#[tokio::test]
async fn control_writes_wait_for_the_control_greeting_and_stop_once_a_read_failed() {
    use kumochi_protocol::message::ControlToShell;

    let ping = frame::encode(&ControlToShell::Ping, Pipe::Control).expect("encodes");
    let order = |error: &io::Error| {
        (
            error.kind(),
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<PairError>())
                .copied(),
        )
    };

    let (mut pair, mut shell) = bound().await;
    let error = pair
        .write_control(&ping)
        .await
        .expect_err("before the greeting");
    assert_eq!(
        order(&error),
        (io::ErrorKind::InvalidData, Some(PairError::Order))
    );
    assert!(*pair.pair.borrow(), "an early write refuses the pair");
    drop(pair);
    let read = bounded(shell.control_in.read(&mut [0; 1])).await;
    assert_eq!(read.expect("closed"), 0, "no byte reached the socket");

    let (mut pair, _shell) = bound().await;
    assert_eq!(
        pair.data_greeting(by()).await.map(|_| ()),
        Err(PairError::Order)
    );
    let error = pair.write_control(&ping).await.expect_err("once spent");
    assert_eq!(
        order(&error),
        (io::ErrorKind::InvalidData, Some(PairError::Order))
    );

    let (mut pair, mut shell) = bound().await;
    shell
        .send(Pipe::Control, &ControlToBrain::Hello(hello()))
        .await;
    pair.control_greeting(by()).await.expect("control greeting");
    for before_data in [true, false] {
        if !before_data {
            shell.send(Pipe::Data, &DataToBrain::Hello(hello())).await;
            pair.data_greeting(by()).await.expect("data greeting");
        }
        pair.write_control(&ping).await.expect("written");
        let got = bounded(ipc::read::<ControlToShell>(
            &mut shell.control_in,
            Pipe::Control,
        ))
        .await;
        assert_eq!(
            got.expect("read"),
            Some(ControlToShell::Ping),
            "{before_data}"
        );
    }
    assert!(!*pair.pair.borrow());
}
