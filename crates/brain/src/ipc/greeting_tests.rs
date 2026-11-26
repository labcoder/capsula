//! The shared greeting over an in-memory transport: real framing and the real capability check,
//! with what the flow reserved, wrote, acquired and bound kept for the assertions.

use std::io;
use std::time::Duration;

use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    AudioSession, CAPABILITY_LEN, Capability, ControlToBrain, ControlToShell, DataToBrain, Hello,
    Incarnation, IssueSession,
};
use kumochi_protocol::tap::Direction;
use kumochi_protocol::{BUILD_ID, Secret};
use tokio::io::AsyncRead;
use tokio::time::{Instant, sleep_until};

use super::{
    HANDSHAKE_TIMEOUT, Legs, PeerAuth, SessionError, Tap, greet, read_sized, startup_hello,
};
use crate::runtime::PairStart;

/// Both legs as byte streams. The data leg finishes acquiring at `open_at`, or at once.
pub(super) struct Memory<C, D> {
    capability: Capability,
    control: C,
    data: D,
    pub(super) open_at: Option<Instant>,
    write_at: Option<Instant>,
    /// Fails every control write, as a pipe the shell already closed.
    pub(super) refuse_writes: bool,
    pub(super) written: Vec<u8>,
    opened: Option<Instant>,
    pub(super) bound: bool,
}

impl<C, D> Memory<C, D> {
    /// Expects the capability `[1; CAPABILITY_LEN]`.
    pub(super) fn new(control: C, data: D) -> Self {
        Self {
            capability: Secret::new([1; CAPABILITY_LEN]),
            control,
            data,
            open_at: None,
            write_at: None,
            refuse_writes: false,
            written: Vec::new(),
            opened: None,
            bound: false,
        }
    }
}

impl<C: AsyncRead + Unpin, D: AsyncRead + Unpin> Legs for Memory<C, D> {
    type Peer = ();
    type Data = D;

    async fn control_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, AudioSession, PairStart)>, SessionError> {
        let Some((first, len)) =
            read_sized::<ControlToBrain>(&mut self.control, Pipe::Control).await?
        else {
            return Ok(None);
        };
        tap.record(Pipe::Control, Direction::ToBrain, &first, len);
        let (hello, audio, start) = startup_hello(first, None)?;
        let auth = PeerAuth::Capability {
            hello,
            expected: &self.capability,
        };
        Ok(Some((auth, audio, start)))
    }

    async fn write_control(&mut self, frame: &[u8]) -> io::Result<()> {
        if self.refuse_writes {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if let Some(at) = self.write_at {
            sleep_until(at).await;
        }
        self.written.extend_from_slice(frame);
        Ok(())
    }

    async fn open_data(&mut self) -> Result<(), SessionError> {
        self.opened = Some(Instant::now());
        if let Some(at) = self.open_at {
            sleep_until(at).await;
        }
        Ok(())
    }

    async fn data_greeting(
        &mut self,
        tap: &mut Tap,
    ) -> Result<Option<(PeerAuth<'_>, Option<IssueSession>)>, SessionError> {
        let Some((first, len)) = read_sized::<DataToBrain>(&mut self.data, Pipe::Data).await?
        else {
            return Ok(None);
        };
        tap.record(Pipe::Data, Direction::ToBrain, &first, len);
        let (hello, session) = match first {
            DataToBrain::Hello(hello) => (hello, None),
            DataToBrain::QueuedHello(hello, session) => (hello, Some(session)),
            _ => return Err(SessionError::ClosedEarly),
        };
        let auth = PeerAuth::Capability {
            hello,
            expected: &self.capability,
        };
        Ok(Some((auth, session)))
    }

    fn bind(&mut self) -> Result<(), SessionError> {
        self.bound = true;
        Ok(())
    }

    fn data(&mut self) -> &mut D {
        &mut self.data
    }
}

fn startup() -> Instant {
    Instant::now() + HANDSHAKE_TIMEOUT
}

const SESSION: IssueSession = IssueSession {
    incarnation: Incarnation(3),
    epoch: 7,
};

fn hello(byte: u8, build: &str) -> Hello {
    Hello {
        build_id: build.to_owned(),
        capability: Secret::new([byte; CAPABILITY_LEN]),
    }
}

fn control_frame(message: &ControlToBrain) -> Vec<u8> {
    frame::encode(message, Pipe::Control).unwrap()
}

fn data_frames(messages: &[DataToBrain]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|message| frame::encode(message, Pipe::Data).unwrap())
        .collect()
}

fn welcome_frame(session: IssueSession) -> Vec<u8> {
    frame::encode(
        &ControlToShell::Welcome {
            incarnation: session.incarnation,
            issue_epoch: session.epoch,
        },
        Pipe::Control,
    )
    .unwrap()
}

/// A data leg that would pass in either mode if it were ever read.
fn ready_data(mode: AudioSession) -> Vec<u8> {
    let greeting = match mode {
        AudioSession::Legacy => DataToBrain::Hello(hello(1, BUILD_ID)),
        AudioSession::QueuedRequired => DataToBrain::QueuedHello(hello(1, BUILD_ID), SESSION),
    };
    data_frames(&[greeting, DataToBrain::TextMetrics(None)])
}

#[tokio::test]
async fn a_refused_control_greeting_reserves_nothing_and_never_opens_data() {
    type Want = fn(&SessionError) -> bool;
    let cases: [(Vec<u8>, Want); 6] = [
        (
            control_frame(&ControlToBrain::Hello(hello(2, BUILD_ID))),
            |e| matches!(e, SessionError::WrongCapability),
        ),
        (
            control_frame(&ControlToBrain::QueuedHello(hello(2, "0.0.0+other"))),
            |e| matches!(e, SessionError::WrongCapability),
        ),
        (control_frame(&ControlToBrain::Quitting), |e| {
            matches!(e, SessionError::NotHello)
        }),
        (Vec::new(), |e| matches!(e, SessionError::ClosedEarly)),
        (vec![1, 0], |e| {
            matches!(e, SessionError::Frame(FrameError::Truncated))
        }),
        (vec![1, 0, 0, 0, 255], |e| {
            matches!(e, SessionError::Frame(FrameError::Malformed(_)))
        }),
    ];
    for (index, (control, want)) in cases.into_iter().enumerate() {
        let data = ready_data(AudioSession::Legacy);
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        let mut reserved = 0;
        let result = greet(&mut legs, &mut Tap::default(), startup(), || {
            reserved += 1;
            SESSION
        })
        .await;
        let error = result.err().expect("a refused greeting fails");
        assert!(want(&error), "case {index}: {error:?}");
        assert_eq!(reserved, 0, "case {index}");
        assert_eq!(legs.written, [0_u8; 0], "case {index}");
        assert!(legs.opened.is_none() && !legs.bound, "case {index}");
    }
}

#[tokio::test]
async fn a_build_mismatch_goes_only_to_the_capability_holder_and_reserves_nothing() {
    let other = hello(1, "0.0.0+other");
    for control in [
        ControlToBrain::Hello(other.clone()),
        ControlToBrain::QueuedHello(other),
    ] {
        let control = control_frame(&control);
        let data = ready_data(AudioSession::Legacy);
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        let mut reserved = 0;
        let result = greet(&mut legs, &mut Tap::default(), startup(), || {
            reserved += 1;
            SESSION
        })
        .await;
        assert!(matches!(result, Ok(None)));
        let reply = ControlToShell::BuildMismatch {
            brain_build: BUILD_ID.to_owned(),
        };
        assert_eq!(legs.written, frame::encode(&reply, Pipe::Control).unwrap());
        assert_eq!(reserved, 0);
        assert!(legs.opened.is_none() && !legs.bound);
    }
}

#[tokio::test]
async fn a_build_mismatch_is_noticed_even_when_its_answer_cant_be_written() {
    let captured = crate::test_support::Captured::default();
    let capturing = captured.install();
    let control = control_frame(&ControlToBrain::Hello(hello(1, "0.0.0+other")));
    let data = ready_data(AudioSession::Legacy);
    let mut legs = Memory::new(control.as_slice(), data.as_slice());
    legs.refuse_writes = true;
    let result = greet(&mut legs, &mut Tap::default(), startup(), || SESSION).await;
    drop(capturing);
    assert!(matches!(
        &result,
        Err(SessionError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe
    ));
    assert!(
        captured.text().contains("build mismatch"),
        "{}",
        captured.text()
    );
}

#[tokio::test]
async fn a_welcome_that_fails_to_send_never_opens_data() {
    for mode in [AudioSession::Legacy, AudioSession::QueuedRequired] {
        let control = control_frame(&match mode {
            AudioSession::Legacy => ControlToBrain::Hello(hello(1, BUILD_ID)),
            AudioSession::QueuedRequired => ControlToBrain::QueuedHello(hello(1, BUILD_ID)),
        });
        let data = ready_data(mode);
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        legs.refuse_writes = true;
        let mut reserved = 0;
        let result = greet(&mut legs, &mut Tap::default(), startup(), || {
            reserved += 1;
            SESSION
        })
        .await;
        assert!(
            matches!(&result, Err(SessionError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe),
            "{mode:?}"
        );
        assert_eq!(reserved, 1, "the session is reserved before Welcome");
        assert!(legs.opened.is_none() && !legs.bound, "{mode:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_pending_welcome_and_the_data_leg_share_the_startup_deadline() {
    let control = control_frame(&ControlToBrain::QueuedHello(hello(1, BUILD_ID)));
    let data = ready_data(AudioSession::QueuedRequired);
    let late = HANDSHAKE_TIMEOUT + Duration::from_secs(1);
    for (welcome_at, open_at) in [(late, late), (Duration::from_secs(1), late)] {
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        let start = Instant::now();
        legs.write_at = Some(start + welcome_at);
        legs.open_at = Some(start + open_at);
        let mut reserved = 0;
        let result = greet(&mut legs, &mut Tap::default(), startup(), || {
            reserved += 1;
            SESSION
        })
        .await;
        let case = format!("welcome at {welcome_at:?}");
        assert!(matches!(result, Err(SessionError::Timeout)), "{case}");
        assert!(
            (HANDSHAKE_TIMEOUT..=HANDSHAKE_TIMEOUT + Duration::from_millis(2))
                .contains(&start.elapsed()),
            "{case}"
        );
        assert_eq!(reserved, 1, "{case}");
        assert!(!legs.bound, "{case}");
        if welcome_at == late {
            assert!(legs.written.is_empty() && legs.opened.is_none(), "{case}");
        } else {
            assert_eq!(legs.written, welcome_frame(SESSION), "{case}");
            assert!(
                legs.opened.expect("data opened") >= start + welcome_at,
                "{case}"
            );
        }
    }
}

#[tokio::test]
async fn both_modes_welcome_the_exact_reserved_session_and_admit_its_echo() {
    for (control, mode) in [
        (
            ControlToBrain::Hello(hello(1, BUILD_ID)),
            AudioSession::Legacy,
        ),
        (
            ControlToBrain::QueuedHello(hello(1, BUILD_ID)),
            AudioSession::QueuedRequired,
        ),
    ] {
        let control = control_frame(&control);
        let data = ready_data(mode);
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        let mut reserved = 0;
        let admitted = greet(&mut legs, &mut Tap::default(), startup(), || {
            reserved += 1;
            SESSION
        })
        .await
        .unwrap()
        .expect("both legs pass");
        assert_eq!(reserved, 1);
        assert_eq!(legs.written, welcome_frame(SESSION), "{mode:?}");
        assert_eq!(admitted.audio, mode);
        assert_eq!(admitted.start, PairStart::Ordinary);
        assert_eq!(admitted.session, SESSION);
        assert_eq!(admitted.text_metrics, None);
        assert!(legs.opened.is_some() && legs.bound, "{mode:?}");
    }
}

#[tokio::test]
async fn a_data_greeting_from_another_welcome_or_mode_is_refused_before_binding() {
    let stale = IssueSession {
        epoch: SESSION.epoch - 1,
        ..SESSION
    };
    let queued = ControlToBrain::QueuedHello(hello(1, BUILD_ID));
    let legacy = ControlToBrain::Hello(hello(1, BUILD_ID));
    for (index, (control, data)) in [
        (&queued, DataToBrain::QueuedHello(hello(1, BUILD_ID), stale)),
        (&queued, DataToBrain::Hello(hello(1, BUILD_ID))),
        (
            &legacy,
            DataToBrain::QueuedHello(hello(1, BUILD_ID), SESSION),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let control = control_frame(control);
        let data = data_frames(&[data, DataToBrain::TextMetrics(None)]);
        let mut legs = Memory::new(control.as_slice(), data.as_slice());
        let result = greet(&mut legs, &mut Tap::default(), startup(), || SESSION).await;
        assert!(
            matches!(
                result,
                Err(SessionError::Protocol("data hello does not match control"))
            ),
            "case {index}"
        );
        assert_eq!(legs.written, welcome_frame(SESSION), "case {index}");
        assert!(legs.opened.is_some() && !legs.bound, "case {index}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_silent_control_leg_times_out_before_anything_is_reserved_or_opened() {
    let (control, _shell) = tokio::io::duplex(64);
    let data = ready_data(AudioSession::Legacy);
    let mut legs = Memory::new(control, data.as_slice());
    let mut reserved = 0;
    let start = Instant::now();
    let result = greet(&mut legs, &mut Tap::default(), startup(), || {
        reserved += 1;
        SESSION
    })
    .await;
    assert!(matches!(result, Err(SessionError::Timeout)));
    assert!(
        (HANDSHAKE_TIMEOUT..=HANDSHAKE_TIMEOUT + Duration::from_millis(2))
            .contains(&start.elapsed())
    );
    assert_eq!(reserved, 0);
    assert!(legs.written.is_empty() && legs.opened.is_none() && !legs.bound);
}
