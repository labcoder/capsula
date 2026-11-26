use std::os::fd::AsRawFd as _;
use std::sync::mpsc;

use kumochi_brain::platform::InheritedFds;
use kumochi_protocol::BUILD_ID;
use kumochi_protocol::constants::{ProductIdentity, SESSION_READY};
use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, Hello, Phase, RunId, ScreenChoice,
};

use super::*;
use crate::platform::macos::{child, pair};

const LOGICAL: usize = 256 * 1024;
const WINDOW: u32 = 512 * 1024;
const STALL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(500);
const CONTROL: Duration = Duration::from_millis(750);
const IO: Duration = Duration::from_secs(1);
const QUIET: Duration = Duration::from_millis(150);
const RUN: RunId = RunId(1);

struct Shell {
    control: PipeClient,
    data: PipeClient,
    finished: mpsc::Receiver<Result<(), kumochi_brain::ParentBoundError>>,
}

impl Shell {
    fn start() -> Self {
        let (ends, inherited) = pair::pairs().expect("socketpairs");
        child::set_socket_int(inherited.data.as_raw_fd(), libc::SO_SNDBUF, 72 * 1024)
            .expect("tight data send buffer");
        child::set_socket_int(ends.data.as_raw_fd(), libc::SO_RCVBUF, 72 * 1024)
            .expect("tight data receive buffer");
        let by = Instant::now() + Duration::from_secs(12);
        let (publish, seed) = mpsc::channel();
        let (exit, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let fds = InheritedFds::new(inherited.control, inherited.data);
            let result = kumochi_brain::serve_pair_credit_fixture(fds, publish, by);
            exit.send(result).ok();
        });
        let seed = seed
            .recv_timeout(by.saturating_duration_since(Instant::now()))
            .expect("the fixture published its synthetic seed");
        ends.control.set_nonblocking(true).expect("bounded READY");
        let mut ready = [0];
        fill(&ends.control, &mut ready, by).expect("READY arrived");
        assert_eq!(ready, [SESSION_READY], "the responder started admission");
        let channel = if ProductIdentity::ACTIVE == ProductIdentity::RELEASE {
            Channel::Release
        } else {
            Channel::Dev
        };
        let (control, data) = initiate(ends, seed.expose(), channel, by).expect("bound pair");
        let shell = Self {
            control,
            data,
            finished,
        };
        let hello = Hello::session(BUILD_ID.to_owned());
        shell.control(&ControlToBrain::Hello(hello.clone()), by);
        shell.control_until(by, |frame| matches!(frame, ControlToShell::Welcome { .. }));
        shell.data(&DataToBrain::Hello(hello), by);
        shell.data(&DataToBrain::TextMetrics(None), by);
        shell.control_until(by, |frame| matches!(frame, ControlToShell::Scenes { .. }));
        shell
    }

    fn control(&self, message: &ControlToBrain, by: Instant) {
        let bytes = frame::encode(message, Pipe::Control).expect("control frame");
        send_frame(&self.control, &bytes, by);
    }

    fn data(&self, message: &DataToBrain, by: Instant) {
        let bytes = frame::encode(message, Pipe::Data).expect("data frame");
        send_frame(&self.data, &bytes, by);
    }

    fn next_control(&self, by: Instant) -> Result<Option<ControlToShell>, FrameError> {
        frame::read(&mut By(&self.control, by), Pipe::Control)
    }

    fn control_until(&self, by: Instant, mut done: impl FnMut(&ControlToShell) -> bool) {
        loop {
            let frame = self
                .next_control(by)
                .expect("control arrived by its deadline")
                .expect("control leg still open");
            if frame == ControlToShell::Ping {
                self.control(&ControlToBrain::Pong, by);
            }
            if done(&frame) {
                return;
            }
        }
    }

    fn submit(&self) -> Instant {
        let sent = Instant::now();
        self.data(
            &DataToBrain::TextSubmit {
                turn: 1,
                thread: None,
                screen: ScreenChoice::None,
                text: "go".into(),
            },
            sent + IO,
        );
        self.control_until(sent + IO, |frame| phase(frame, Phase::Answering));
        sent
    }

    fn credit(&self, bytes: u32) {
        self.data(&DataToBrain::Credit { bytes }, Instant::now() + IO);
    }

    fn frame(&self, letter: u8) {
        let by = Instant::now() + IO;
        let mut bytes = vec![0; LOGICAL];
        let mut filled = 0;
        for expected in [
            MAX_PLAINTEXT,
            MAX_PLAINTEXT,
            MAX_PLAINTEXT,
            MAX_PLAINTEXT,
            68,
        ] {
            let n = By(&self.data, by)
                .read(&mut bytes[filled..])
                .expect("a credited frame's record");
            assert_eq!(n, expected, "five records in one logical frame");
            filled += n;
        }
        assert_eq!(
            filled, LOGICAL,
            "all record plaintext fits one logical frame"
        );
        let mut encoded = bytes.as_slice();
        let Some(DataToShell::AssistantText { run, delta }) =
            frame::read(&mut encoded, Pipe::Data).expect("whole frame")
        else {
            panic!("expected synthetic run text");
        };
        assert_eq!(run, RUN, "text belongs to the submitted run");
        assert!(
            encoded.is_empty(),
            "the five records contain exactly one frame"
        );
        assert!(
            delta.bytes().all(|byte| byte == letter),
            "the requested frame's synthetic text arrived intact and in order",
        );
    }

    fn no_data(&self, reason: &str) {
        let mut byte = [0];
        let error = By(&self.data, Instant::now() + QUIET)
            .read(&mut byte)
            .expect_err(reason);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{reason}");
    }

    fn record(&self) {
        let mut bytes = vec![0; MAX_PLAINTEXT];
        let n = By(&self.data, Instant::now() + IO)
            .read(&mut bytes)
            .expect("a record of the stalled frame");
        assert_eq!(n, MAX_PLAINTEXT, "only one full record was read");
    }

    fn quit(self) {
        let by = Instant::now() + IO * 3;
        self.control(&ControlToBrain::Quitting, by);
        assert_eq!(
            self.finished
                .recv_timeout(by.saturating_duration_since(Instant::now())),
            Ok(Ok(())),
            "the fixture settled the authenticated Quit",
        );
    }
}

fn send_frame(mut leg: &PipeClient, bytes: &[u8], by: Instant) {
    let left = by.saturating_duration_since(Instant::now());
    assert!(!left.is_zero(), "frame send deadline");
    leg.set_timeout(Some(left));
    assert_eq!(
        leg.write(bytes).expect("whole frame written"),
        bytes.len(),
        "the shell wrote the entire logical frame",
    );
}

fn phase(frame: &ControlToShell, expected: Phase) -> bool {
    matches!(frame, ControlToShell::RunState { run: RUN, phase, .. } if *phase == expected)
}

#[test]
fn credit_is_exactly_two_logical_frames_and_control_survives_a_stalled_data_writer() {
    let shell = Shell::start();
    shell.submit();
    shell.frame(b'a');
    shell.frame(b'b');
    shell.no_data("initial credit must stop at 512 KiB");

    shell.credit(WINDOW / 2 - 1);
    shell.no_data("one byte short must not send a fragment");
    shell.credit(1);
    shell.frame(b'c');

    shell.credit(u32::MAX);
    shell.frame(b'd');
    let before_fifth = Instant::now();
    shell.frame(b'e');
    shell.no_data("a grant must not increase the 512 KiB window");

    shell.credit(WINDOW / 2);
    shell.record();
    for _ in 0..2 {
        shell.control_until(Instant::now() + CONTROL, |frame| {
            *frame == ControlToShell::Ping
        });
    }
    let by = (Instant::now() + TICK).min(before_fifth + STALL);
    shell.control(
        &ControlToBrain::Stop {
            run: RUN,
            revealed: Vec::new(),
        },
        by,
    );
    shell.control_until(by, |frame| phase(frame, Phase::Interrupted));
    assert!(
        Instant::now() < before_fifth + STALL,
        "Stop, not STALL, ended the run"
    );
    shell.quit();
}

#[test]
fn fragment_progress_does_not_restart_the_logical_frame_stall_deadline() {
    let shell = Shell::start();
    let submitted = shell.submit();
    shell.record();
    let started = Instant::now();
    let progress_at = started + Duration::from_millis(1250);
    loop {
        match shell.next_control(progress_at) {
            Ok(Some(ControlToShell::Ping)) => {
                shell.control(&ControlToBrain::Pong, Instant::now() + CONTROL);
            }
            Ok(Some(frame)) => assert!(
                !phase(&frame, Phase::Interrupted),
                "STALL must not fire at a fragment boundary",
            ),
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::TimedOut => break,
            _ => panic!("control leg ended before fragment progress"),
        }
    }
    shell.record();
    shell.control_until(started + STALL + CONTROL, |frame| {
        phase(frame, Phase::Interrupted)
    });
    assert!(
        Instant::now() >= submitted + STALL,
        "the two-second STALL fired early"
    );
    shell.quit();
}
