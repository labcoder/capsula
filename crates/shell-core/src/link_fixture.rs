//! What the link's tests and the presence's tests share: a connection the link attends on its
//! own thread, and readers of its events that give up after a deadline.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(debug_assertions)]
use std::time::Instant;

use async_channel::Receiver;
#[cfg(debug_assertions)]
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{AudioSession, Incarnation, IssueSession, ShellTuning};
#[cfg(debug_assertions)]
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, Narration, Phase, RunId, RunOrigin, ThreadId,
};

use super::{
    Attendance, Connection, Link, LinkEvent, LinkHandle, LinkStart, Session, Shared, start,
};
use crate::platform::{PipeClient, test_pair};

/// How long a test waits for anything before it fails.
pub(crate) const PATIENCE: Duration = Duration::from_secs(5);

pub use crate::speaker::queued::{FixturePlayback, await_fake_disposal, serialize_playback};

pub fn with_outbox_held(handle: &LinkHandle, session: &Session, run: impl FnOnce()) {
    with_link_locks_held(handle, session, Hold::Outbox, run);
}

#[cfg(debug_assertions)]
pub fn with_outbox_and_output_held(handle: &LinkHandle, session: &Session, run: impl FnOnce()) {
    with_link_locks_held(handle, session, Hold::Both, run);
}

enum Hold {
    Outbox,
    #[cfg(debug_assertions)]
    Both,
}

fn with_link_locks_held(handle: &LinkHandle, session: &Session, locks: Hold, run: impl FnOnce()) {
    let (ready, held) = mpsc::channel();
    let (release, done) = mpsc::channel();
    thread::scope(|scope| {
        let holder = scope.spawn(move || {
            let outbox = handle
                .shared
                .outbox
                .lock()
                .expect("the outbox lock is healthy");
            assert_eq!(
                outbox.as_ref().expect("a linked outbox").generation,
                session.generation(),
                "hold the exact connected outbox"
            );
            let held = || {
                ready
                    .send(())
                    .expect("the headless caller waits for the lock");
                done.recv_timeout(Duration::from_secs(2))
            };
            match locks {
                Hold::Outbox => held(),
                #[cfg(debug_assertions)]
                Hold::Both => handle.shared.output.with_locked(held),
            }
        });
        held.recv_timeout(PATIENCE)
            .expect("the fixture holds the outbox");
        run();
        release
            .send(())
            .expect("Esc returned while the real outbox was held");
        holder
            .join()
            .expect("the fixture holder ran")
            .expect("Esc did not wait for the outbox");
    });
}

#[cfg(debug_assertions)]
pub struct Authenticated {
    pub handle: LinkHandle,
    pub events: Receiver<LinkEvent>,
    pub playback: mpsc::Receiver<FixturePlayback>,
    pub link: JoinHandle<bool>,
}

#[cfg(debug_assertions)]
impl Authenticated {
    pub fn hold_text(&self) {
        self.handle
            .shared
            .hold_text_writer
            .store(1, std::sync::atomic::Ordering::Release);
    }

    pub fn text_held(&self) -> bool {
        self.handle
            .shared
            .hold_text_writer
            .load(std::sync::atomic::Ordering::Acquire)
            == 2
    }

    pub fn release_text(&self) {
        self.handle
            .shared
            .hold_text_writer
            .store(0, std::sync::atomic::Ordering::Release);
    }
}

/// Opens the next run's fake device before its worker admits PCM.
#[cfg(debug_assertions)]
pub fn prime_next_playback(handle: &LinkHandle) {
    handle
        .shared
        .prime_fake_playback
        .store(true, std::sync::atomic::Ordering::Release);
}

/// The real authenticated Shell path, without opening the vault or a physical device.
#[cfg(debug_assertions)]
pub fn authenticated(
    id: u32,
    hello: kumochi_protocol::message::Hello,
    mode: AudioSession,
) -> Authenticated {
    authenticated_with_output(id, hello, mode, Shared::default())
}

/// Gives the first named run an open fake device before it can prime any source.
#[cfg(debug_assertions)]
pub fn authenticated_primed(id: u32, hello: kumochi_protocol::message::Hello) -> Authenticated {
    authenticated_with_output(
        id,
        hello,
        AudioSession::QueuedRequired,
        Shared {
            prime_fake_playback: std::sync::atomic::AtomicBool::new(true),
            ..Shared::default()
        },
    )
}

#[cfg(debug_assertions)]
/// Selects narration exclusively for the offline authenticated test pair.
pub fn authenticated_narrated(id: u32, hello: kumochi_protocol::message::Hello) -> Authenticated {
    authenticated_with_output(
        id,
        hello,
        AudioSession::QueuedRequired,
        Shared {
            narrated: true,
            ..Shared::default()
        },
    )
}

#[cfg(debug_assertions)]
fn authenticated_with_output(
    id: u32,
    hello: kumochi_protocol::message::Hello,
    mode: AudioSession,
    mut shared: Shared,
) -> Authenticated {
    use kumochi_protocol::frame::Pipe;

    use crate::platform::{handshake_pair, open};
    let (sender, events) = async_channel::unbounded();
    let (output, playback) = mpsc::sync_channel(1);
    shared.fake_playback = output.into();
    let link = Link {
        brain: std::env::current_exe().expect("the fixture shares the test image"),
        session: id,
        audio: mode,
        shared: Arc::new(shared),
    };
    let (handle, start) = start(link, sender);
    let link = thread::spawn(move || {
        let end = Instant::now() + super::CONNECT_TIMEOUT;
        let attempt = (mode == AudioSession::QueuedRequired).then_some(end);
        let open = |pipe: Pipe| open(&pipe.name(id), &start.link.brain, attempt);
        let control = open(Pipe::Control).expect("the fixture control pipe");
        control.set_timeout(Some(PATIENCE));
        let connection = handshake_pair(
            control,
            hello,
            mode,
            None,
            attempt,
            super::DrawingText::default(),
            || open(Pipe::Data),
        )
        .expect("authenticate both fixture legs");
        start
            .link
            .attend_until(connection, None, &start.events, end, &Arc::default())
            == Attendance::Lost
    });
    Authenticated {
        handle,
        events,
        playback,
        link,
    }
}

pub(crate) fn pair(label: &str) -> (PipeClient, PipeClient) {
    let name = format!(
        r"\\.\pipe\{}-test-{}-{label}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    test_pair(&name).expect("a test pipe")
}

/// A connection the link attends on its own thread, and the brain's ends of its pipes.
#[cfg_attr(
    not(any(test, debug_assertions)),
    expect(dead_code, reason = "only this crate's tests read the brain's ends")
)]
pub struct Attended {
    pub handle: LinkHandle,
    pub events: Receiver<LinkEvent>,
    pub(crate) control: PipeClient,
    pub(crate) data: Arc<PipeClient>,
    /// Returns whether the UI was still there when the connection ended.
    pub link: JoinHandle<bool>,
}

impl Attended {
    /// Reads one frame from the connected test peer with its existing timeout.
    ///
    /// # Panics
    /// Panics if the peer closes or a frame cannot be read.
    pub fn next_control(&self) -> kumochi_protocol::message::ControlToBrain {
        use kumochi_protocol::frame::{self, Pipe};
        frame::read(&mut &self.control, Pipe::Control)
            .expect("a valid control receipt")
            .expect("the pipe is connected")
    }

    /// Checks the peer's exact shutdown frames, closes the peer and joins the link.
    /// # Panics
    /// Panics on an unexpected frame, pipe error or link failure.
    #[cfg(debug_assertions)]
    pub fn verify_quit(self, expected: [kumochi_protocol::message::ControlToBrain; 3]) {
        use kumochi_protocol::frame::{self, FrameError, Pipe};
        use kumochi_protocol::message::ControlToBrain;
        use windows::Win32::Foundation::ERROR_TIMEOUT;

        for message in expected {
            assert_eq!(
                frame::read(&mut &self.control, Pipe::Control).expect("a shutdown frame"),
                Some(message),
                "only the expected shutdown control is written"
            );
        }
        self.control.set_timeout(Some(Duration::from_millis(100)));
        assert!(
            matches!(
                frame::read::<ControlToBrain>(&mut &self.control, Pipe::Control),
                Err(FrameError::Io(error)) if error.raw_os_error() == Some(ERROR_TIMEOUT.to_hresult().0)
            ),
            "no later control follows Quitting"
        );
        drop(self.control);
        drop(self.data);
        assert!(
            self.link.join().expect("link joined"),
            "the peer closed after Quit"
        );
    }
}

pub fn attended(label: &str) -> Attended {
    attended_from(label, 0)
}

pub(super) fn attended_from(label: &str, previous_generation: u64) -> Attended {
    attended_shared(
        label,
        Arc::new(Shared {
            generations: std::sync::atomic::AtomicU64::new(previous_generation),
            ..Shared::default()
        }),
        Incarnation(1),
    )
}

pub(super) fn attended_shared(
    label: &str,
    shared: Arc<Shared>,
    incarnation: Incarnation,
) -> Attended {
    attended_with(label, shared, incarnation, AudioSession::Legacy)
}

#[cfg(test)]
pub(super) fn attended_queued(label: &str) -> Attended {
    attended_queued_shared(label, Arc::default())
}

#[cfg(test)]
pub(super) fn attended_queued_shared(label: &str, shared: Arc<Shared>) -> Attended {
    attended_with(label, shared, Incarnation(1), AudioSession::QueuedRequired)
}

pub(super) fn attended_with(
    label: &str,
    shared: Arc<Shared>,
    incarnation: Incarnation,
    audio: AudioSession,
) -> Attended {
    let (sender, events) = async_channel::unbounded();
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        audio,
        shared,
    };
    attended_prepared(label, start(link, sender), events, incarnation)
}

/// Attends a prepared link's own start, handle and event receiver against the scripted peer.
pub(super) fn attended_prepared(
    label: &str,
    (handle, start): (LinkHandle, LinkStart),
    events: Receiver<LinkEvent>,
    incarnation: Incarnation,
) -> Attended {
    let (control, shell_control) = pair(&format!("{label}-control"));
    let (data, shell_data) = pair(&format!("{label}-data"));
    control.set_timeout(Some(PATIENCE));
    data.set_timeout(Some(PATIENCE));
    let connection = Connection {
        control: Arc::new(shell_control),
        data: Arc::new(shell_data),
        issue_origin: IssueSession {
            incarnation,
            epoch: 91,
        },
        drawing_font_family: None,
        tuning: ShellTuning::default(),
    };
    let link = thread::spawn(move || {
        let result = start.link.attend(connection, &start.events);
        drop(start.alive);
        result == Attendance::Lost
    });
    Attended {
        handle,
        events,
        control,
        data: Arc::new(data),
        link,
    }
}

/// The link's events with a timeout on each.
pub fn patient(events: Receiver<LinkEvent>) -> mpsc::Receiver<LinkEvent> {
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

pub fn next(events: &mpsc::Receiver<LinkEvent>) -> LinkEvent {
    events.recv_timeout(PATIENCE).expect("an event in time")
}

pub fn connected(event: LinkEvent) -> Session {
    let LinkEvent::Connected(session) = event else {
        panic!("Connected first, got {event:?}");
    };
    session
}

#[cfg(debug_assertions)]
pub(super) fn settings_view() -> kumochi_protocol::message::SettingsView {
    use kumochi_protocol::message::{AssistantName, SettingsView};
    SettingsView {
        assistant_name: AssistantName::new("Cloud").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(9),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
        shortcut: kumochi_protocol::message::ShortcutView::Default,
    }
}

#[cfg(debug_assertions)]
pub(super) fn run_state(
    run: u64,
    thread: ThreadId,
    phase: Phase,
    origin: Option<RunOrigin>,
) -> ControlToShell {
    ControlToShell::RunState {
        origin,
        run: RunId(run),
        thread,
        phase,
        error: None,
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn queued_registration(
    control: &PipeClient,
    shared: &Shared,
) -> kumochi_protocol::message::OutputGeneration {
    queued_registration_as(control, declared(shared))
}

#[cfg(all(test, debug_assertions))]
fn declared(shared: &Shared) -> Narration {
    if shared.narrated {
        Narration::Conducted
    } else {
        Narration::Plain
    }
}

/// The pair's talk-back registration, which must declare `narration` on the wire.
#[cfg(debug_assertions)]
pub(super) fn queued_registration_as(
    control: &PipeClient,
    narration: Narration,
) -> kumochi_protocol::message::OutputGeneration {
    frame::write(
        &mut &*control,
        &ControlToShell::Settings {
            view: kumochi_protocol::message::SettingsView {
                talk_back: true,
                ..settings_view()
            },
            answer: false,
        },
        Pipe::Control,
    )
    .expect("talk-back settings written");
    match frame::read(&mut &*control, Pipe::Control).expect("read registration") {
        Some(ControlToBrain::QueuedAudioOutput(24_000, output, declared))
            if declared == narration =>
        {
            output
        }
        other => panic!("a 24 kHz queued registration, got {other:?}"),
    }
}

#[cfg(debug_assertions)]
pub(super) fn acknowledge_queued(
    control: &PipeClient,
    output: kumochi_protocol::message::OutputGeneration,
) {
    frame::write(
        &mut &*control,
        &ControlToShell::QueuedAudioRegistered(output),
        Pipe::Control,
    )
    .expect("registered");
    frame::write(&mut &*control, &ControlToShell::Ping, Pipe::Control).expect("ping");
    assert_eq!(
        frame::read(&mut &*control, Pipe::Control).expect("peer reads Pong"),
        Some(ControlToBrain::Pong),
        "the control reader has handled the prior ACK"
    );
}

#[cfg(debug_assertions)]
pub(super) fn text_ready(events: &std::sync::mpsc::Receiver<LinkEvent>) {
    assert!(
        matches!(next(events), LinkEvent::TextReady { .. }),
        "the acknowledged pair admits turns"
    );
}

#[cfg(debug_assertions)]
pub(super) fn queued_text(turn: u32, text: &str) -> DataToBrain {
    DataToBrain::TextSubmit {
        turn,
        thread: Some(ThreadId(1)),
        screen: kumochi_protocol::message::ScreenChoice::None,
        text: text.into(),
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn queued_named_run(
    link: &Attended,
    events: &std::sync::mpsc::Receiver<LinkEvent>,
) -> (Session, kumochi_protocol::message::OutputGeneration) {
    queued_named_run_as(link, events, declared(&link.handle.shared))
}

#[cfg(debug_assertions)]
pub(super) fn queued_named_run_as(
    link: &Attended,
    events: &std::sync::mpsc::Receiver<LinkEvent>,
    narration: Narration,
) -> (Session, kumochi_protocol::message::OutputGeneration) {
    let session = connected(next(events));
    let output = queued_registration_as(&link.control, narration);
    acknowledge_queued(&link.control, output);
    let text = queued_text(1, "old owner");
    assert!(
        link.handle.send_data(&session, text.clone()),
        "the old pair admits its first turn"
    );
    assert_eq!(
        frame::read(&mut &*link.data, Pipe::Data).expect("the old data pipe"),
        Some(text),
        "the old peer sees the exact typed submit"
    );
    frame::write(
        &mut &link.control,
        &run_state(
            7,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        ),
        Pipe::Control,
    )
    .expect("the old peer names the accepted turn");
    assert!(
        matches!(next(events), LinkEvent::Settings { .. }),
        "settings preceded the run"
    );
    text_ready(events);
    assert!(
        matches!(next(events), LinkEvent::RunState { run: RunId(7), .. }),
        "the Shell bound the old worker"
    );
    (session, output)
}
