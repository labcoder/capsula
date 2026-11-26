use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::SESSION_READY;
use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    AssistantName, AudioSession, ControlToBrain, ControlToShell, DataToBrain, DockPosition, Hello,
    Incarnation, IssueSession, MaxFrameRate, OnboardingStep, SettingsView, ShellTuning, SttUnload,
    ThreadId,
};
use kumochi_session::{
    BrainAwaitsInit, Channel, ENVELOPE_LEN, MAX_PLAINTEXT, SECRET_LEN, Session, SessionError,
};

use super::super::NotStarted;
use super::super::wipes;
use super::*;
use crate::link::DrawingText;
use crate::platform::Connection;
use crate::platform::SpawnWord;
use crate::platform::connect::{LinkError, after_ready, connect_pair, not_started, over_pair};

/// A synthetic group secret; no real key is ever used here.
pub(in crate::platform::macos) const SECRET: [u8; SECRET_LEN] = [0x5A; SECRET_LEN];
const BOUND: Duration = Duration::from_secs(10);
const SHORT: Duration = Duration::from_millis(60);
const GAP: Duration = Duration::from_millis(40);

fn by() -> Instant {
    Instant::now() + BOUND
}

/// The shell's ends and the brain's, as the spawn would leave them.
pub(in crate::platform::macos) fn ends() -> (ShellEnds, UnixStream, UnixStream) {
    let (shell, child) = super::super::pair::pairs().expect("pairs");
    (
        shell,
        UnixStream::from(child.control),
        UnixStream::from(child.data),
    )
}

fn handshake_message(
    io: &mut UnixStream,
    len: impl FnOnce([u8; ENVELOPE_LEN]) -> Result<usize, SessionError>,
) -> Vec<u8> {
    let mut envelope = [0; ENVELOPE_LEN];
    io.read_exact(&mut envelope).expect("envelope");
    let mut message = vec![0; len(envelope).expect("length")];
    io.read_exact(&mut message).expect("message");
    message
}

/// The brain's side of a bound pair, driven by hand.
pub(in crate::platform::macos) struct Brain {
    control: UnixStream,
    data: UnixStream,
    session: Session,
}

/// The honest responder over the brain's two ends.
fn respond(mut control: UnixStream, mut data: UnixStream) -> Brain {
    for end in [&control, &data] {
        end.set_read_timeout(Some(BOUND)).expect("bounded");
    }
    let mut brain = BrainAwaitsInit::new(&SECRET, Channel::Dev).expect("brain starts");
    let init = handshake_message(&mut control, |envelope| brain.record_len(envelope));
    let (mut brain, reply) = brain.receive(&init).expect("init");
    control.write_all(&reply.bytes).expect("reply");
    let finished = handshake_message(&mut control, |envelope| brain.record_len(envelope));
    let brain = brain.receive(&finished).expect("Finished");
    let init = handshake_message(&mut data, |envelope| brain.record_len(envelope));
    let (brain, reply) = brain.receive(&init).expect("data init");
    data.write_all(&reply.bytes).expect("data reply");
    let finished = handshake_message(&mut data, |envelope| brain.record_len(envelope));
    let brain = brain.receive(&finished).expect("data Finished");
    let bind = handshake_message(&mut control, |envelope| brain.record_len(envelope));
    let (session, _proof, ack) = brain.receive(&bind).expect("Bind");
    control.write_all(&ack.bytes).expect("BindAck");
    Brain {
        control,
        data,
        session,
    }
}

impl Brain {
    fn sealed(&mut self, pipe: Pipe, frame: &[u8]) -> Vec<u8> {
        let sealer = match pipe {
            Pipe::Control => &mut self.session.control.sealer,
            Pipe::Data => &mut self.session.data.sealer,
        };
        let mut wire = Vec::new();
        for record in frame.chunks(MAX_PLAINTEXT) {
            sealer.seal(record).expect("seals");
            let bytes = sealer.unwritten().expect("sealed").to_vec();
            sealer.advance(bytes.len()).expect("taken");
            wire.extend(bytes);
        }
        wire
    }

    fn send(&mut self, pipe: Pipe, frame: &[u8]) {
        let wire = self.sealed(pipe, frame);
        let io = match pipe {
            Pipe::Control => &mut self.control,
            Pipe::Data => &mut self.data,
        };
        io.write_all(&wire).expect("sent");
    }

    fn control(&mut self, message: &ControlToShell) {
        self.send(
            Pipe::Control,
            &frame::encode(message, Pipe::Control).expect("encodes"),
        );
    }

    fn receive(&mut self, pipe: Pipe) -> Vec<u8> {
        let (io, opener) = match pipe {
            Pipe::Control => (&mut self.control, &mut self.session.control.opener),
            Pipe::Data => (&mut self.data, &mut self.session.data.opener),
        };
        let mut frame = Vec::new();
        loop {
            let body = handshake_message(io, |envelope| opener.record_len(envelope));
            let delivery = opener.open(&body).expect("opens");
            frame.extend_from_slice(delivery.bytes());
            if delivery.frame_done() {
                return frame;
            }
        }
    }

    pub(in crate::platform::macos) fn control_message(&mut self) -> ControlToBrain {
        frame::decode(&self.receive(Pipe::Control)[HEADER_LEN..]).expect("decodes")
    }

    fn data_message(&mut self) -> DataToBrain {
        frame::decode(&self.receive(Pipe::Data)[HEADER_LEN..]).expect("decodes")
    }
}

/// Both legs bound, the brain's side returned for the test to drive.
fn bound() -> (PipeClient, PipeClient, Brain) {
    let (shell, brain_control, brain_data) = ends();
    let brain = thread::spawn(move || respond(brain_control, brain_data));
    let (control, data) =
        initiate(shell, Secret(Box::new(SECRET)), Channel::Dev, by()).expect("initiated");
    (
        control,
        data,
        brain.join().expect("the brain side finished"),
    )
}

/// A frame of `total` bytes, header included, with a counting payload.
fn counting_frame(total: usize) -> Vec<u8> {
    let payload = total - HEADER_LEN;
    let mut bytes = u32::try_from(payload).expect("u32").to_le_bytes().to_vec();
    bytes.extend((0..payload).map(|i| u8::try_from(i % 251).expect("under 251")));
    bytes
}

#[test]
fn frames_cross_both_legs_once_the_initiator_has_bound_the_pair() {
    let (control, data, mut brain) = bound();
    control.set_timeout(Some(BOUND));
    data.set_timeout(Some(BOUND));
    frame::write(&mut &control, &ControlToBrain::Pong, Pipe::Control).expect("written");
    assert_eq!(brain.control_message(), ControlToBrain::Pong);
    brain.control(&ControlToShell::Ping);
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("read"),
        Some(ControlToShell::Ping)
    );
    let large = counting_frame(3 * MAX_PLAINTEXT + 7);
    let wire = brain.sealed(Pipe::Data, &large);
    let mut io = brain.data.try_clone().expect("cloned");
    let writer = thread::spawn(move || io.write_all(&wire));
    let mut got = vec![0; large.len()];
    (&data).read_exact(&mut got).expect("the whole frame");
    assert_eq!(got, large);
    writer.join().expect("the writer returned").expect("sent");
}

#[test]
fn a_read_that_times_out_mid_record_resumes_it() {
    let (control, _data, mut brain) = bound();
    let wire = brain.sealed(
        Pipe::Control,
        &frame::encode(&ControlToShell::Ping, Pipe::Control).expect("encodes"),
    );
    let (first, rest) = wire.split_at(wire.len() / 2);
    brain.control.write_all(first).expect("first half");
    control.set_timeout(Some(SHORT));
    let mut byte = [0; 1];
    let timed_out = (&control).read(&mut byte).expect_err("half a record");
    assert!(matches!(
        timed_out.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(!control.is_aborted(), "a timeout isn't a refusal");
    brain.control.write_all(rest).expect("second half");
    control.set_timeout(Some(BOUND));
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("read"),
        Some(ControlToShell::Ping)
    );
}

#[test]
fn bytes_that_arent_one_frame_refuse_the_pair() {
    let (control, data, _brain) = bound();
    let mut frame = frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
    frame.push(0);
    let refused = (&control).write(&frame).expect_err("one byte too many");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
    assert!(control.is_aborted() && data.is_aborted(), "both legs");
}

#[test]
fn a_record_that_fails_to_authenticate_refuses_and_wakes_both_legs() {
    let (control, data, mut brain) = bound();
    let data = std::sync::Arc::new(data);
    let blocked = thread::spawn({
        let data = std::sync::Arc::clone(&data);
        move || {
            data.set_timeout(Some(BOUND));
            let mut byte = [0; 1];
            (&*data).read(&mut byte)
        }
    });
    let mut forged = vec![0, 32];
    forged.extend([0xAB; 32]);
    // Let the data reader reach its socket wait first.
    thread::sleep(SHORT);
    let started = Instant::now();
    brain.control.write_all(&forged).expect("forged record");
    control.set_timeout(Some(BOUND));
    let refused = frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect_err("forged");
    assert!(
        matches!(refused, frame::FrameError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData)
    );
    let woke = blocked.join().expect("the data reader returned");
    assert!(
        started.elapsed() < BOUND / 2,
        "the refusal woke the data reader"
    );
    assert_eq!(
        woke.expect_err("refused, not an end").kind(),
        std::io::ErrorKind::BrokenPipe
    );
    let mut byte = [0; 1];
    assert_eq!(
        brain
            .data
            .read(&mut byte)
            .expect("the brain reads its data end"),
        0,
        "the shell shut its data end down"
    );
    assert!(control.is_aborted() && data.is_aborted(), "both legs");
}

#[test]
fn the_stream_ending_inside_a_record_is_truncated_and_refuses() {
    // One byte of the envelope, and all of the record but its last byte.
    for kept in [|_: usize| 1, |len: usize| len - 1] {
        let (control, data, mut brain) = bound();
        let wire = brain.sealed(
            Pipe::Control,
            &frame::encode(&ControlToShell::Ping, Pipe::Control).expect("encodes"),
        );
        brain
            .control
            .write_all(&wire[..kept(wire.len())])
            .expect("part of a record");
        drop(brain);
        control.set_timeout(Some(BOUND));
        let refused =
            frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect_err("cut off");
        assert!(
            matches!(refused, frame::FrameError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData)
        );
        assert!(data.is_aborted(), "the pair refused");
    }
}

#[test]
fn the_stream_ending_between_frames_is_a_clean_end() {
    let (control, data, brain) = bound();
    drop(brain);
    control.set_timeout(Some(BOUND));
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("clean"),
        None
    );
    assert!(!data.is_aborted(), "an ordinary end isn't a refusal");
}

#[test]
fn abort_ends_only_its_own_leg() {
    let (control, data, mut brain) = bound();
    control.abort();
    assert!(control.is_aborted() && !data.is_aborted());
    let frame = counting_frame(HEADER_LEN + 9);
    brain.send(Pipe::Data, &frame);
    data.set_timeout(Some(BOUND));
    let mut got = vec![0; frame.len()];
    (&data).read_exact(&mut got).expect("data still flows");
    assert_eq!(got, frame);
}

#[test]
fn the_initiator_ends_at_its_deadline() {
    let (shell, _brain_control, _brain_data) = ends();
    let deadline = Instant::now() + SHORT;
    let refused = initiate(shell, Secret(Box::new(SECRET)), Channel::Dev, deadline)
        .expect_err("nobody answers");
    assert!(matches!(
        refused.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(Instant::now() >= deadline, "waited for the deadline");
    assert!(
        Instant::now() < deadline + Duration::from_secs(1),
        "and no longer"
    );
}

#[test]
fn a_brain_with_another_secret_never_binds() {
    let (shell, mut brain_control, brain_data) = ends();
    let brain = thread::spawn(move || {
        let mut brain =
            BrainAwaitsInit::new(&[0xA5; SECRET_LEN], Channel::Dev).expect("brain starts");
        let init = handshake_message(&mut brain_control, |envelope| brain.record_len(envelope));
        assert_eq!(brain.receive(&init).err(), Some(SessionError::Handshake));
        drop((brain_control, brain_data));
    });
    let refused =
        initiate(shell, Secret(Box::new(SECRET)), Channel::Dev, by()).expect_err("no reply");
    assert_eq!(refused.kind(), std::io::ErrorKind::UnexpectedEof);
    brain.join().expect("the brain side finished");
}

#[test]
fn a_peeked_frame_is_read_again_first() {
    let (control, _data, mut brain) = bound();
    let settings = ControlToShell::Settings {
        view: view(),
        answer: false,
    };
    brain.control(&settings);
    brain.control(&ControlToShell::Ping);
    control.set_timeout(Some(BOUND));
    assert_eq!(
        control.peek_control(by()).expect("peeked"),
        Some(settings.clone())
    );
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("read"),
        Some(settings)
    );
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("read"),
        Some(ControlToShell::Ping)
    );
}

fn view() -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    }
}

const ORIGIN: IssueSession = IssueSession {
    incarnation: Incarnation(5),
    epoch: 2,
};

/// What the brain does after binding, up to the frame it sends once it has committed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::platform::macos) enum Commit {
    Settings,
    Closes,
    Pings,
    Stalls,
    /// Settings a byte at a time, each gap well inside any one transfer's bound.
    Trickles,
}

/// Runs the session-path attempt against a scripted brain and returns its result.
fn attempt(
    audio: AudioSession,
    commit: Commit,
    deadline: Instant,
) -> (Result<Connection, LinkError>, thread::JoinHandle<Brain>) {
    let (shell, brain_control, brain_data) = ends();
    let brain = thread::spawn(move || scripted(brain_control, brain_data, audio, commit, deadline));
    let drawing = DrawingText {
        profile: None,
        font_family: None,
    };
    let result = over_pair(
        shell,
        Secret(Box::new(SECRET)),
        Channel::Dev,
        audio,
        None,
        deadline,
        drawing,
    );
    (result, brain)
}

/// The brain's side of an attempt over its two ends: the honest responder, both greetings and
/// then `commit`.
pub(in crate::platform::macos) fn scripted(
    brain_control: UnixStream,
    brain_data: UnixStream,
    audio: AudioSession,
    commit: Commit,
    deadline: Instant,
) -> Brain {
    let mut brain = respond(brain_control, brain_data);
    let hello = Hello::session(BUILD_ID.to_owned());
    let expected = match audio {
        AudioSession::Legacy => ControlToBrain::Hello(hello.clone()),
        AudioSession::QueuedRequired => ControlToBrain::QueuedHello(hello.clone()),
    };
    assert_eq!(brain.control_message(), expected);
    brain.control(&ControlToShell::Welcome {
        incarnation: ORIGIN.incarnation,
        issue_epoch: ORIGIN.epoch,
    });
    let expected = match audio {
        AudioSession::Legacy => DataToBrain::Hello(hello),
        AudioSession::QueuedRequired => DataToBrain::QueuedHello(hello, ORIGIN),
    };
    assert_eq!(brain.data_message(), expected);
    assert_eq!(brain.data_message(), DataToBrain::TextMetrics(None));
    brain.control(&ControlToShell::ShellTuning {
        session: ORIGIN,
        tuning: ShellTuning::default(),
    });
    match commit {
        Commit::Settings => brain.control(&ControlToShell::Settings {
            view: view(),
            answer: false,
        }),
        Commit::Pings => brain.control(&ControlToShell::Ping),
        Commit::Closes => brain
            .control
            .shutdown(std::net::Shutdown::Both)
            .expect("closed"),
        Commit::Trickles => {
            let settings = frame::encode(
                &ControlToShell::Settings {
                    view: view(),
                    answer: false,
                },
                Pipe::Control,
            )
            .expect("encodes");
            for byte in brain.sealed(Pipe::Control, &settings) {
                thread::sleep(GAP);
                if brain.control.write_all(&[byte]).is_err() {
                    break;
                }
            }
        }
        Commit::Stalls => {
            thread::sleep(deadline.saturating_duration_since(Instant::now()) + SHORT);
        }
    }
    brain
}

#[test]
fn the_attempt_completes_on_the_first_settings_frame_in_both_modes() {
    for audio in [AudioSession::Legacy, AudioSession::QueuedRequired] {
        let (result, brain) = attempt(audio, Commit::Settings, by());
        let connection = result.expect("connected");
        assert_eq!(connection.issue_origin, ORIGIN);
        brain.join().expect("the brain side finished");
        assert!(
            matches!(
                frame::read::<ControlToShell>(&mut &*connection.control, Pipe::Control),
                Ok(Some(ControlToShell::Settings { .. }))
            ),
            "the link reads Settings first"
        );
    }
}

#[test]
fn no_connection_without_settings_first() {
    for commit in [Commit::Closes, Commit::Pings] {
        for audio in [AudioSession::Legacy, AudioSession::QueuedRequired] {
            let (result, brain) = attempt(audio, commit, by());
            assert!(matches!(result, Err(LinkError::Rejected)));
            brain.join().expect("the brain side finished");
        }
    }
}

#[test]
fn the_attempt_ends_at_its_deadline_while_waiting_for_settings() {
    let deadline = Instant::now() + Duration::from_millis(500);
    let (result, brain) = attempt(AudioSession::Legacy, Commit::Stalls, deadline);
    assert!(matches!(result, Err(LinkError::ConnectExpired)));
    assert!(Instant::now() >= deadline, "waited for the deadline");
    brain.join().expect("the brain side finished");
}

#[test]
fn the_settings_wait_ends_at_the_deadline_however_the_frame_trickles() {
    let deadline = Instant::now() + Duration::from_millis(700);
    let (result, brain) = attempt(AudioSession::Legacy, Commit::Trickles, deadline);
    assert!(matches!(result, Err(LinkError::ConnectExpired)));
    assert!(
        Instant::now() < deadline + Duration::from_millis(400),
        "ended near the deadline, not when the frame finished"
    );
    brain.join().expect("the brain side finished");
}

#[test]
fn the_initiator_ends_at_its_deadline_however_the_reply_trickles() {
    let (shell, mut brain_control, brain_data) = ends();
    let brain = thread::spawn(move || {
        let mut brain = BrainAwaitsInit::new(&SECRET, Channel::Dev).expect("brain starts");
        let init = handshake_message(&mut brain_control, |envelope| brain.record_len(envelope));
        let (_brain, reply) = brain.receive(&init).expect("init");
        for byte in &reply.bytes {
            thread::sleep(GAP);
            if brain_control.write_all(&[*byte]).is_err() {
                break;
            }
        }
        drop(brain_data);
    });
    let deadline = Instant::now() + Duration::from_millis(400);
    let refused =
        initiate(shell, Secret(Box::new(SECRET)), Channel::Dev, deadline).expect_err("too slow");
    assert!(matches!(
        refused.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(
        Instant::now() < deadline + Duration::from_millis(400),
        "ended near the deadline, not when the reply finished"
    );
    brain.join().expect("the brain side finished");
}

#[test]
fn a_peeked_frame_isnt_delivered_once_the_pair_refuses() {
    let (control, data, mut brain) = bound();
    brain.control(&ControlToShell::Settings {
        view: view(),
        answer: false,
    });
    assert!(matches!(
        control.peek_control(by()),
        Ok(Some(ControlToShell::Settings { .. }))
    ));
    let mut forged = vec![0, 32];
    forged.extend([0xAB; 32]);
    brain.data.write_all(&forged).expect("forged record");
    data.set_timeout(Some(BOUND));
    let mut byte = [0; 1];
    assert_eq!(
        (&data).read(&mut byte).expect_err("forged").kind(),
        std::io::ErrorKind::InvalidData
    );
    let mut buf = [0x77; 8];
    assert_eq!(
        (&control).read(&mut buf).expect_err("refused").kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(buf, [0x77; 8], "nothing delivered");
}

#[test]
fn an_aborted_leg_fails_its_reads_and_leaves_the_other_leg_working() {
    let (control, data, mut brain) = bound();
    let wire = brain.sealed(
        Pipe::Control,
        &frame::encode(&ControlToShell::Ping, Pipe::Control).expect("encodes"),
    );
    brain
        .control
        .write_all(&wire[..wire.len() / 2])
        .expect("half a record");
    control.set_timeout(Some(SHORT));
    let mut byte = [0; 1];
    assert!((&control).read(&mut byte).is_err(), "half a record");
    control.abort();
    assert_eq!(
        (&control).read(&mut byte).expect_err("aborted").kind(),
        std::io::ErrorKind::ConnectionAborted
    );
    assert!(!data.is_aborted(), "its own shutdown isn't a truncation");
    data.set_timeout(Some(BOUND));
    frame::write(&mut &data, &DataToBrain::TextMetrics(None), Pipe::Data).expect("written");
    assert_eq!(brain.data_message(), DataToBrain::TextMetrics(None));
}

#[test]
fn a_frame_larger_than_a_record_leaves_the_shell_whole() {
    let (_control, data, mut brain) = bound();
    data.set_timeout(Some(BOUND));
    let large = counting_frame(3 * MAX_PLAINTEXT + 7);
    let writer = thread::spawn(move || (&data).write(&large).map(|n| (n, large)));
    let got = brain.receive(Pipe::Data);
    let (n, large) = writer
        .join()
        .expect("the writer returned")
        .expect("written");
    assert_eq!((n, got), (large.len(), large));
}

#[test]
fn a_write_that_fails_refuses_the_pair_for_good() {
    let (control, data, brain) = bound();
    drop(brain);
    data.set_timeout(Some(BOUND));
    let large = counting_frame(3 * MAX_PLAINTEXT + 7);
    assert!((&data).write(&large).is_err(), "the brain is gone");
    assert!(control.is_aborted() && data.is_aborted(), "both legs");
    let small = frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
    assert_eq!(
        (&control).write(&small).expect_err("still refused").kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[test]
fn an_expired_attempt_starts_no_brain() {
    let result = connect_pair(
        std::path::Path::new("/nonexistent/kumochid"),
        std::sync::Arc::new(SpawnWord::default()),
        Channel::Dev,
        AudioSession::Legacy,
        None,
        Instant::now(),
        |_| panic!("no metrics for an expired attempt"),
    );
    assert!(matches!(
        result,
        Err(crate::platform::Failed {
            error: LinkError::ConnectExpired,
            child: Some(crate::platform::Unconnected::NotSpawned),
        })
    ));
}

#[test]
fn a_bounded_write_ends_at_its_deadline_while_the_brain_drains_slowly() {
    let (control, data, mut brain) = bound();
    // Small buffers, so one record can't fit whatever the host's defaults.
    for (fd, buffer) in [
        (data.stream.as_raw_fd(), libc::SO_SNDBUF),
        (brain.data.as_raw_fd(), libc::SO_RCVBUF),
    ] {
        super::super::child::set_socket_int(fd, buffer, 8192).expect("buffer size");
    }
    // Less than any send's worth per wake, for a second at most.
    let drained = thread::spawn(move || {
        let mut chunk = [0; 64];
        let mut total = 0;
        let stop = Instant::now() + Duration::from_secs(1);
        while Instant::now() < stop {
            thread::sleep(Duration::from_millis(50));
            match brain.data.read(&mut chunk) {
                Ok(0) | Err(_) => return total,
                Ok(n) => total += n,
            }
        }
        total
    });
    let timeout = Duration::from_millis(300);
    data.set_timeout(Some(timeout));
    let large = counting_frame(MAX_PLAINTEXT);
    let started = Instant::now();
    let failed = (&data).write(&large).expect_err("drained too slowly");
    assert!(
        started.elapsed() < timeout + Duration::from_millis(300),
        "ended near its deadline, not when the brain caught up"
    );
    assert!(matches!(
        failed.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(
        control.is_aborted() && data.is_aborted(),
        "a frame cut off part way refuses the pair"
    );
    let small = frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
    assert_eq!(
        (&control).write(&small).expect_err("still refused").kind(),
        std::io::ErrorKind::BrokenPipe
    );
    drop((control, data));
    assert!(
        drained.join().expect("the brain side finished") > 0,
        "part of the frame reached the brain"
    );
}

/// Whether a leg's reader holds no bytes and no opener.
fn wiped(client: &PipeClient) -> bool {
    clear(client.reader())
}

fn clear(reader: &std::sync::Mutex<Reader>) -> bool {
    let reader = locked(reader);
    reader.opener.is_none()
        && reader.record.iter().all(|&byte| byte == 0)
        && (reader.filled, reader.start, reader.end) == (0, 0, 0)
        && reader.retained.is_empty()
}

#[test]
fn a_refusal_wipes_both_readers_without_another_read() {
    let (control, data, mut brain) = bound();
    control.set_timeout(Some(BOUND));
    data.set_timeout(Some(BOUND));
    brain.control(&ControlToShell::Ping);
    let mut byte = [0; 1];
    (&control)
        .read_exact(&mut byte)
        .expect("the frame's first byte");
    assert!(!wiped(&control), "the rest of the frame waits to be read");
    let mut forged = vec![0, 32];
    forged.extend([0xAB; 32]);
    brain.data.write_all(&forged).expect("forged record");
    assert_eq!(
        (&data).read(&mut byte).expect_err("forged").kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(
        wiped(&control) && wiped(&data),
        "both readers, before any later read"
    );
}

#[test]
fn a_refusal_wipes_a_peeked_frame_without_another_read() {
    let (control, data, mut brain) = bound();
    brain.control(&ControlToShell::Settings {
        view: view(),
        answer: false,
    });
    assert!(matches!(
        control.peek_control(by()),
        Ok(Some(ControlToShell::Settings { .. }))
    ));
    assert!(!wiped(&control), "the peeked frame is kept");
    let mut forged = vec![0, 32];
    forged.extend([0xAB; 32]);
    brain.data.write_all(&forged).expect("forged record");
    data.set_timeout(Some(BOUND));
    let mut byte = [0; 1];
    assert!((&data).read(&mut byte).is_err(), "forged");
    assert!(wiped(&control), "the peeked frame is gone");
}

#[test]
fn abort_wipes_its_own_reader_without_another_read() {
    let (control, data, mut brain) = bound();
    control.set_timeout(Some(BOUND));
    data.set_timeout(Some(BOUND));
    brain.control(&ControlToShell::Ping);
    brain.send(Pipe::Data, &counting_frame(HEADER_LEN + 9));
    let mut byte = [0; 1];
    (&control)
        .read_exact(&mut byte)
        .expect("control's first byte");
    (&data).read_exact(&mut byte).expect("data's first byte");
    control.abort();
    assert!(wiped(&control), "the aborted leg");
    assert!(!wiped(&data), "the other leg keeps its frame");
}

#[test]
fn a_dropped_leg_wipes_its_reader_while_the_other_lives() {
    let (control, data, mut brain) = bound();
    control.set_timeout(Some(BOUND));
    brain.control(&ControlToShell::Ping);
    let mut byte = [0; 1];
    (&control)
        .read_exact(&mut byte)
        .expect("the frame's first byte");
    assert!(!wiped(&control), "the rest of the frame waits to be read");
    drop(control);
    assert!(clear(&data.pair.readers[0]), "the dropped leg's reader");
    assert!(!data.is_aborted(), "dropping a leg isn't a refusal");
}

#[test]
fn a_pending_spawn_report_is_the_deadline_only_once_it_has_passed() {
    let passed = Instant::now();
    assert!(matches!(
        not_started(NotStarted::Pending, passed),
        LinkError::ConnectExpired
    ));
    assert!(matches!(
        not_started(NotStarted::Pending, by()),
        LinkError::Launch(ref error) if error.kind() == std::io::ErrorKind::TimedOut
    ));
}

#[test]
fn nothing_moves_once_the_call_deadline_has_passed() {
    let (control, data, mut brain) = bound();
    brain.control(&ControlToShell::Ping);
    control.set_timeout(Some(Duration::ZERO));
    let mut byte = [0; 1];
    assert_eq!(
        (&control)
            .read(&mut byte)
            .expect_err("past its deadline")
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    control.set_timeout(Some(BOUND));
    assert_eq!(
        frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect("read"),
        Some(ControlToShell::Ping),
        "the queued frame is still whole"
    );
    data.set_timeout(Some(Duration::ZERO));
    let frame = counting_frame(HEADER_LEN + 9);
    assert_eq!(
        (&data).write(&frame).expect_err("past its deadline").kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(
        brain
            .data
            .read(&mut byte)
            .expect("the brain reads its data end"),
        0,
        "no byte of the frame reached the brain"
    );
}

/// What one run of the shell's side after the spawn did.
struct Spawned {
    result: Result<Connection, LinkError>,
    /// How often the fake item reader ran.
    reads: usize,
    /// Whether the reader's buffer held only zeros as it was freed; None if never read or freed.
    wiped: Option<bool>,
}

/// Runs the shell's side after the spawn over `shell`, with a fake item reader that fills the
/// synthetic secret, takes `lag` and answers `item`. It fills even when it refuses, so a wipe
/// on a refusal is seen.
fn after_spawn(
    shell: ShellEnds,
    deadline: Instant,
    item: Result<(), kumochi_storage::ItemError>,
    lag: Duration,
) -> Spawned {
    after_spawn_hardened(shell, deadline, item, lag, || Ok(()))
}

/// [`after_spawn`] with `harden` in place of the real core-dump limit, which only a re-exec child
/// may change, so the shared test process keeps its limits.
fn after_spawn_hardened(
    shell: ShellEnds,
    deadline: Instant,
    item: Result<(), kumochi_storage::ItemError>,
    lag: Duration,
    harden: impl FnOnce() -> std::io::Result<()>,
) -> Spawned {
    let reads = std::cell::Cell::new(0);
    let buffer = std::cell::Cell::new(None);
    let result = after_ready(
        shell,
        Channel::Dev,
        AudioSession::Legacy,
        None,
        deadline,
        |_| DrawingText::default(),
        harden,
        |into| {
            reads.set(reads.get() + 1);
            buffer.set(Some((wipes::len(), into.as_ptr().addr())));
            *into = SECRET;
            thread::sleep(lag);
            item
        },
        || false,
    );
    Spawned {
        result,
        reads: reads.get(),
        wiped: buffer.get().and_then(|(since, at)| wipes::found(since, at)),
    }
}

/// Reads both brain ends to their close and asserts the shell wrote nothing on either.
fn nothing_written(control: &mut UnixStream, data: &mut UnixStream) {
    for end in [control, data] {
        let mut written = Vec::new();
        end.read_to_end(&mut written)
            .expect("the shell's end closed");
        assert_eq!(written, Vec::<u8>::new(), "no initiator bytes");
    }
}

/// Both brain ends bounded, then READY queued. Darwin refuses the option once the peer has
/// closed, so it's set first.
fn ready_queued() -> (ShellEnds, UnixStream, UnixStream) {
    let (shell, mut brain_control, brain_data) = ends();
    for end in [&brain_control, &brain_data] {
        end.set_read_timeout(Some(BOUND)).expect("bounded");
    }
    brain_control.write_all(&[SESSION_READY]).expect("READY");
    (shell, brain_control, brain_data)
}

#[test]
fn a_failed_hardening_reads_no_item_and_writes_nothing() {
    let (shell, mut brain_control, mut brain_data) = ready_queued();
    let spawned = after_spawn_hardened(shell, by(), Ok(()), Duration::ZERO, || {
        Err(std::io::ErrorKind::PermissionDenied.into())
    });
    assert!(matches!(spawned.result, Err(LinkError::Harden)));
    assert_eq!((spawned.reads, spawned.wiped), (0, None));
    nothing_written(&mut brain_control, &mut brain_data);
}

const CORE_DUMPS_ENTRY: &str = "platform::macos::leg::tests::core_dumps_entry";

/// The re-exec child's exact arguments. The entry does nothing under any others, so a plain
/// `--ignored` run never lowers the test process's own core limit.
const CORE_DUMPS_ARGS: [&str; 4] = ["--exact", CORE_DUMPS_ENTRY, "--ignored", "--test-threads=1"];

/// The entry's exit code once every check passed; libtest never exits with it.
const CORE_DUMPS_RAN: i32 = 87;

/// Run only by [`the_item_is_read_only_after_core_dumps_are_off`], in its own process: it lowers
/// that process's core limit for good, then exits with [`CORE_DUMPS_RAN`].
#[test]
#[ignore = "runs in a re-exec child: it lowers its process's core limit"]
fn core_dumps_entry() {
    use super::super::core_dumps::core_limit;
    use super::super::limit_core_dumps;

    if !std::env::args_os()
        .skip(1)
        .eq(CORE_DUMPS_ARGS.map(std::ffi::OsString::from))
    {
        return;
    }
    let (shell, brain_control, brain_data) = ready_queued();
    drop((brain_control, brain_data));
    let hardened = std::cell::Cell::new(false);
    let seen = std::cell::Cell::new(None);
    let result = after_ready(
        shell,
        Channel::Dev,
        AudioSession::Legacy,
        None,
        by(),
        |_| DrawingText::default(),
        || {
            hardened.set(true);
            limit_core_dumps()
        },
        |into| {
            seen.set(Some((hardened.get(), core_limit().ok())));
            *into = SECRET;
            Ok(())
        },
        || false,
    );
    assert!(result.is_err(), "the brain's ends closed after READY");
    assert_eq!(
        seen.get(),
        Some((true, Some((0, 0)))),
        "hardened before the read"
    );
    assert_eq!(
        limit_core_dumps().map_err(|error| error.kind()),
        Ok(()),
        "idempotent"
    );
    std::process::exit(CORE_DUMPS_RAN);
}

#[test]
fn the_item_is_read_only_after_core_dumps_are_off() {
    use std::process::{Command, Stdio};

    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .args(CORE_DUMPS_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the re-exec child starts");
    let by = by();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait on the re-exec child") {
            break status;
        }
        if Instant::now() >= by {
            let stopped = child.kill().is_ok() || child.try_wait().is_ok_and(|s| s.is_some());
            assert!(stopped, "could not stop the late re-exec child");
            let reap_by = Instant::now() + BOUND;
            while child
                .try_wait()
                .expect("wait on the killed child")
                .is_none()
            {
                assert!(
                    Instant::now() < reap_by,
                    "the killed re-exec child was reaped"
                );
                thread::sleep(Duration::from_millis(10));
            }
            panic!("the re-exec child didn't finish within the bound");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.code(),
        Some(CORE_DUMPS_RAN),
        "the entry ran every check"
    );
}

#[test]
fn ready_then_the_item_runs_the_initiator_to_settings() {
    let (shell, mut brain_control, brain_data) = ends();
    let brain = thread::spawn(move || {
        brain_control.write_all(&[SESSION_READY]).expect("READY");
        scripted(
            brain_control,
            brain_data,
            AudioSession::Legacy,
            Commit::Settings,
            by(),
        )
    });
    let spawned = after_spawn(shell, by(), Ok(()), Duration::ZERO);
    let connection = spawned.result.expect("connected");
    assert_eq!(
        (connection.issue_origin, spawned.reads, spawned.wiped),
        (ORIGIN, 1, Some(true))
    );
    brain.join().expect("the brain side finished");
    assert!(
        matches!(
            frame::read::<ControlToShell>(&mut &*connection.control, Pipe::Control),
            Ok(Some(ControlToShell::Settings { .. }))
        ),
        "the link reads Settings first"
    );
}

#[test]
fn ready_reads_one_byte_and_leaves_the_next_for_the_initiator() {
    let (shell, mut brain_control, _brain_data) = ends();
    brain_control
        .write_all(&[SESSION_READY, 0xEE])
        .expect("READY and a sentinel");
    assert_eq!(read_ready(&shell, by()).expect("read"), Some(SESSION_READY));
    let mut next = [0; 1];
    assert_eq!(
        (&shell.control)
            .read(&mut next)
            .expect("the sentinel is still queued"),
        1
    );
    assert_eq!(next, [0xEE]);
}

#[test]
fn the_shell_writes_nothing_before_ready() {
    let (shell, mut brain_control, mut brain_data) = ends();
    let brain = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        for end in [&brain_control, &brain_data] {
            end.set_nonblocking(true).expect("nonblocking");
        }
        let mut byte = [0; 1];
        for end in [&mut brain_control, &mut brain_data] {
            assert_eq!(
                end.read(&mut byte).expect_err("nothing written").kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        for end in [&brain_control, &brain_data] {
            end.set_nonblocking(false).expect("blocking");
        }
        brain_control.write_all(&[SESSION_READY]).expect("READY");
        scripted(
            brain_control,
            brain_data,
            AudioSession::Legacy,
            Commit::Settings,
            by(),
        )
    });
    let spawned = after_spawn(shell, by(), Ok(()), Duration::ZERO);
    brain.join().expect("the brain saw nothing before READY");
    assert_eq!(
        (
            spawned.result.expect("connected").issue_origin,
            spawned.reads
        ),
        (ORIGIN, 1)
    );
}

#[test]
fn a_first_byte_other_than_ready_is_rejected_before_the_item() {
    for first in [0x00, 0x53] {
        let (shell, mut brain_control, _brain_data) = ends();
        brain_control.write_all(&[first]).expect("first byte");
        let spawned = after_spawn(shell, by(), Ok(()), Duration::ZERO);
        assert!(
            matches!(spawned.result, Err(LinkError::Rejected)),
            "first byte {first:#04x}"
        );
        assert_eq!(spawned.reads, 0, "first byte {first:#04x}");
    }
}

#[test]
fn the_stream_ending_before_ready_is_rejected_before_the_item() {
    let (shell, brain_control, _brain_data) = ends();
    drop(brain_control);
    let spawned = after_spawn(shell, by(), Ok(()), Duration::ZERO);
    assert!(matches!(spawned.result, Err(LinkError::Rejected)));
    assert_eq!(spawned.reads, 0);
}

#[test]
fn the_ready_wait_ends_at_the_deadline_before_the_item() {
    let (shell, brain_control, brain_data) = ends();
    let deadline = Instant::now() + Duration::from_millis(300);
    let brain = thread::spawn(move || {
        thread::sleep(Duration::from_secs(1) + deadline.saturating_duration_since(Instant::now()));
        drop((brain_control, brain_data));
    });
    let spawned = after_spawn(shell, deadline, Ok(()), Duration::ZERO);
    let ended = Instant::now();
    assert!(matches!(spawned.result, Err(LinkError::ConnectExpired)));
    assert_eq!(spawned.reads, 0);
    assert!(ended >= deadline, "waited for the deadline");
    assert!(
        ended < deadline + Duration::from_millis(400),
        "ended near the deadline, not when the brain let go"
    );
    brain.join().expect("the brain side finished");
}

#[test]
fn a_refused_item_read_refuses_with_nothing_written() {
    let (shell, mut brain_control, mut brain_data) = ready_queued();
    let spawned = after_spawn(
        shell,
        by(),
        Err(kumochi_storage::ItemError::Missing),
        Duration::ZERO,
    );
    assert!(matches!(
        spawned.result,
        Err(LinkError::Item(kumochi_storage::ItemError::Missing))
    ));
    assert_eq!((spawned.reads, spawned.wiped), (1, Some(true)));
    nothing_written(&mut brain_control, &mut brain_data);
}

#[test]
fn an_item_read_that_ends_past_the_deadline_expires_whatever_it_answered() {
    for item in [Ok(()), Err(kumochi_storage::ItemError::Missing)] {
        let (shell, mut brain_control, mut brain_data) = ready_queued();
        let deadline = Instant::now() + Duration::from_millis(200);
        let spawned = after_spawn(shell, deadline, item, Duration::from_millis(300));
        assert!(
            matches!(spawned.result, Err(LinkError::ConnectExpired)),
            "{item:?}"
        );
        assert_eq!((spawned.reads, spawned.wiped), (1, Some(true)), "{item:?}");
        nothing_written(&mut brain_control, &mut brain_data);
    }
}

#[test]
fn the_secret_is_wiped_before_the_initiator_sends_its_first_message() {
    let (shell, mut brain_control, brain_data) = ends();
    let secret = Secret(Box::new(SECRET));
    let (since, at) = (wipes::len(), secret.0.as_ptr().addr());
    let peer = thread::spawn(move || {
        brain_control
            .set_read_timeout(Some(BOUND))
            .expect("bounded");
        let mut brain = BrainAwaitsInit::new(&SECRET, Channel::Dev).expect("brain starts");
        handshake_message(&mut brain_control, |envelope| brain.record_len(envelope));
        let wiped = wipes::found(since, at);
        drop((brain_control, brain_data));
        wiped
    });
    initiate(shell, secret, Channel::Dev, by()).expect_err("the peer closes after msg 1");
    assert_eq!(
        peer.join().expect("the peer read msg 1"),
        Some(true),
        "wiped before msg 1 reached the peer"
    );
}
