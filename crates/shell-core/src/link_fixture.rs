//! What the link's tests and the presence's tests share: a connection the link attends on its
//! own thread, and readers of its events that give up after a deadline.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_channel::Receiver;
use kumochi_protocol::message::{Incarnation, IssueSession};

use super::{Connection, Link, LinkEvent, LinkHandle, Session, Shared, start};
use crate::platform::{PipeClient, test_pair};

/// How long a test waits for anything before it fails.
pub(crate) const PATIENCE: Duration = Duration::from_secs(5);

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
    attended_with(label, shared, incarnation, None)
}

/// A connection whose link opens a speaker with `opener`.
#[cfg(test)]
pub(super) fn attended_speaking(label: &str, opener: crate::speaker::Opener) -> Attended {
    attended_with(label, Arc::default(), Incarnation(1), Some(opener))
}

fn attended_with(
    label: &str,
    shared: Arc<Shared>,
    incarnation: Incarnation,
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
    let link = thread::spawn(move || start.link.attend(connection, &start.events));
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
