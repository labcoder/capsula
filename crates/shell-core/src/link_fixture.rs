//! What the link's tests and the presence's tests share: a connection the link attends on its
//! own thread, and readers of its events that give up after a deadline.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_channel::Receiver;
use kumochi_protocol::message::{AudioSession, Incarnation, IssueSession};

use super::{Attendance, Connection, Link, LinkEvent, LinkHandle, Session, Shared, start};
use crate::platform::{PipeClient, test_pair};

/// How long a test waits for anything before it fails.
pub(crate) const PATIENCE: Duration = Duration::from_secs(5);

#[cfg(debug_assertions)]
pub use crate::speaker::queued::{FixturePlayback, await_fake_disposal, serialize_playback};

#[cfg(debug_assertions)]
pub fn with_outbox_held(handle: &LinkHandle, session: &Session, run: impl FnOnce()) {
    with_link_locks_held(handle, session, Hold::Outbox, run);
}

#[cfg(debug_assertions)]
pub fn with_outbox_and_output_held(handle: &LinkHandle, session: &Session, run: impl FnOnce()) {
    with_link_locks_held(handle, session, Hold::Both, run);
}

#[cfg(debug_assertions)]
enum Hold {
    Outbox,
    Both,
}

#[cfg(debug_assertions)]
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
    let (sender, events) = async_channel::unbounded();
    let (output, playback) = mpsc::sync_channel(1);
    shared.fake_playback = Some(output);
    let link = Link {
        brain: std::env::current_exe().expect("the fixture shares the test image"),
        session: id,
        audio: mode,
        shared: Arc::new(shared),
        speaker: None,
    };
    let (handle, start) = start(link, sender);
    let link = thread::spawn(move || {
        let open = |pipe: Pipe| super::open(&pipe.name(id), &start.link.brain);
        let control = open(Pipe::Control).expect("the fixture control pipe");
        control.set_timeout(Some(PATIENCE));
        let connection = super::handshake_pair(control, hello, mode, || open(Pipe::Data))
            .expect("authenticate both fixture legs");
        start.link.attend(connection, &start.events) == Attendance::Lost
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
    not(test),
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
    attended_with(label, shared, incarnation, AudioSession::Legacy, None)
}

/// A connection whose link opens a speaker with `opener`.
#[cfg(test)]
pub(super) fn attended_speaking(label: &str, opener: crate::speaker::Opener) -> Attended {
    attended_with(
        label,
        Arc::default(),
        Incarnation(1),
        AudioSession::Legacy,
        Some(opener),
    )
}

#[cfg(test)]
pub(super) fn attended_queued(label: &str) -> Attended {
    attended_queued_shared(label, Arc::default())
}

#[cfg(test)]
pub(super) fn attended_queued_shared(label: &str, shared: Arc<Shared>) -> Attended {
    attended_with(
        label,
        shared,
        Incarnation(1),
        AudioSession::QueuedRequired,
        None,
    )
}

fn attended_with(
    label: &str,
    shared: Arc<Shared>,
    incarnation: Incarnation,
    audio: AudioSession,
    speaker: Option<crate::speaker::Opener>,
) -> Attended {
    let (control, shell_control) = pair(&format!("{label}-control"));
    let (data, shell_data) = pair(&format!("{label}-data"));
    control.set_timeout(Some(PATIENCE));
    data.set_timeout(Some(PATIENCE));
    let (sender, events) = async_channel::unbounded();
    let link = Link {
        brain: PathBuf::new(),
        session: 0,
        audio,
        shared,
        speaker,
    };
    let (handle, start) = start(link, sender);
    let connection = Connection {
        control: Arc::new(shell_control),
        data: Arc::new(shell_data),
        issue_origin: IssueSession {
            incarnation,
            epoch: 91,
        },
    };
    let link =
        thread::spawn(move || start.link.attend(connection, &start.events) == Attendance::Lost);
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
