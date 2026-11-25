use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::BUILD_ID;
use kumochi_protocol::frame::{self, HEADER_LEN, Pipe};
use kumochi_protocol::message::{
    AssistantName, AudioSession, ControlToBrain, ControlToShell, DataToBrain, DockPosition, Hello,
    Incarnation, IssueSession, MaxFrameRate, OnboardingStep, SettingsView, SttUnload, ThreadId,
};
use kumochi_session::{
    BrainAwaitsInit, Channel, ENVELOPE_LEN, MAX_PLAINTEXT, SECRET_LEN, Session, SessionError,
};

use super::*;
use crate::link::DrawingText;
use crate::platform::Connection;
use crate::platform::connect::{LinkError, over_pair};

/// A synthetic group secret; no real key is ever used here.
const SECRET: [u8; SECRET_LEN] = [0x5A; SECRET_LEN];
const BOUND: Duration = Duration::from_secs(10);
const SHORT: Duration = Duration::from_millis(60);

fn by() -> Instant {
    Instant::now() + BOUND
}

/// The shell's ends and the brain's, as the spawn would leave them.
fn ends() -> (ShellEnds, UnixStream, UnixStream) {
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
struct Brain {
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

    fn control_message(&mut self) -> ControlToBrain {
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
    let (control, data) = initiate(shell, &SECRET, Channel::Dev, by()).expect("initiated");
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
    brain.control.write_all(&forged).expect("forged record");
    control.set_timeout(Some(BOUND));
    let refused = frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect_err("forged");
    assert!(
        matches!(refused, frame::FrameError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData)
    );
    let woke = blocked.join().expect("the data reader returned");
    assert!(!matches!(woke, Ok(1)), "the data leg delivered nothing");
    assert!(control.is_aborted() && data.is_aborted(), "both legs");
}

#[test]
fn the_stream_ending_inside_a_record_is_truncated_and_refuses() {
    let (control, data, mut brain) = bound();
    brain
        .control
        .write_all(&[0, 40, 1, 2, 3])
        .expect("part of a record");
    drop(brain);
    control.set_timeout(Some(BOUND));
    let refused = frame::read::<ControlToShell>(&mut &control, Pipe::Control).expect_err("cut off");
    assert!(
        matches!(refused, frame::FrameError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData)
    );
    assert!(data.is_aborted(), "the pair refused");
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
    let refused = initiate(shell, &SECRET, Channel::Dev, deadline).expect_err("nobody answers");
    assert!(matches!(
        refused.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert!(Instant::now() >= deadline, "waited for the deadline");
    assert!(Instant::now() < deadline + BOUND, "and no longer");
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
    let refused = initiate(shell, &SECRET, Channel::Dev, by()).expect_err("no reply");
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
        control.peek_control().expect("peeked"),
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
enum Commit {
    Settings,
    Closes,
    Pings,
    Stalls,
}

/// Runs the session-path attempt against a scripted brain and returns its result.
fn attempt(
    audio: AudioSession,
    commit: Commit,
    deadline: Instant,
) -> (Result<Connection, LinkError>, thread::JoinHandle<Brain>) {
    let (shell, brain_control, brain_data) = ends();
    let brain = thread::spawn(move || {
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
            Commit::Stalls => {
                thread::sleep(deadline.saturating_duration_since(Instant::now()) + SHORT);
            }
        }
        brain
    });
    let drawing = DrawingText {
        profile: None,
        font_family: None,
    };
    let result = over_pair(shell, &SECRET, Channel::Dev, audio, None, deadline, drawing);
    (result, brain)
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
