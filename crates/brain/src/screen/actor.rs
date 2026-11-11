//! Dormant worker-local ownership loop; A6b2b alone may route production requests here.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use kumochi_protocol::frame;
use kumochi_protocol::geometry::DisplayRef;
use kumochi_protocol::message::ThreadId;
use serde::{Deserialize, Serialize};

use super::capture::{Capturer, Failure, Screen};
use super::elements::{ElementTree, OpenElements, Wake};
use super::native::{CaptureThread, Job};
use super::supervisor::Screen as Kind;
use super::wire::{CaptureId, ElementTarget, Exclusion, FrameMeta, Refusal, TicketWire};
use crate::supervise::{Kind as _, read_request, write_message};

const QUEUE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Owner {
    pub(super) thread: ThreadId,
    pub(super) deletion_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Key {
    pub(super) owner: Owner,
    pub(super) capture: CaptureId,
}

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Context {
        key: Key,
        ticket: TicketWire,
        display: DisplayRef,
        exclude: Exclusion,
    },
    Elements {
        key: Key,
        target: ElementTarget,
    },
    Drop(Key),
    Retire(Owner),
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
enum Reply {
    Pong,
    Acquired(Key, FrameMeta),
    Complete(Key),
    Elements(Key, ElementTree),
    Released(Owner, Option<CaptureId>),
    Refused(Refusal),
    Busy,
    Fault(i32),
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScreenRequest(***)")
    }
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScreenReply(***)")
    }
}

enum Event {
    Request(u64, Request, Instant),
    Wake,
    End(Result<(), frame::FrameError>),
}

fn wake(inbox: SyncSender<Event>) -> Wake {
    Box::new(move || {
        !matches!(
            inbox.try_send(Event::Wake),
            Err(TrySendError::Disconnected(_))
        )
    })
}

struct Capture {
    id: u64,
    key: Key,
    ticket: TicketWire,
    display: DisplayRef,
    received: Instant,
    expires: Instant,
}

pub(super) fn serve_on(
    mut input: impl std::io::Read + Send + 'static,
    output: &mut impl std::io::Write,
    open: impl FnMut() -> Result<Box<dyn Capturer>, i32> + Send + 'static,
    elements: OpenElements,
    now: impl Fn() -> Instant + Send + Sync + 'static,
) -> Result<(), frame::FrameError> {
    let (inbox, events) = mpsc::sync_channel(QUEUE);
    let capture = CaptureThread::open(open, wake(inbox.clone()))
        .map_err(|_| std::io::Error::other("capture thread"))?;
    let mut screen = Screen::default();
    screen.reader = Some(elements(wake(inbox.clone())));
    let now = std::sync::Arc::new(now);
    let clock = std::sync::Arc::clone(&now);
    drop(std::thread::spawn(move || {
        let ended = loop {
            match read_request::<Request>(&mut input, Kind::MAX_REQUEST) {
                Ok(Some((id, body))) => {
                    if inbox.send(Event::Request(id, body, clock())).is_err() {
                        return;
                    }
                }
                end => break end.map(|_| ()),
            }
        };
        drop(inbox.send(Event::End(ended)));
    }));
    run(&events, output, capture, screen, &*now)
}

#[expect(
    clippy::too_many_lines,
    reason = "one event turn keeps revocation before publication"
)]
fn run(
    events: &Receiver<Event>,
    output: &mut impl std::io::Write,
    mut native: CaptureThread,
    mut screen: Screen,
    now: &dyn Fn() -> Instant,
) -> Result<(), frame::FrameError> {
    let mut fences = BTreeMap::<u64, u64>::new();
    let mut current = None;
    let mut live = false;
    let mut expires = None;
    let mut capturing: Option<Capture> = None;
    let mut walking: Option<(u64, Key, Instant)> = None;
    let mut releasing = Vec::new();
    loop {
        let deadline = expires
            .into_iter()
            .chain(walking.map(|(_, _, until)| until))
            .min();
        let event = match deadline {
            Some(until) => events
                .recv_timeout(until.saturating_duration_since(now()))
                .ok(),
            None => events.recv().ok(),
        };
        if expires.is_some_and(|until| now() >= until) {
            live = false;
            expires = None;
            screen.clear();
        }
        let mut send = |id, reply| write_message(output, id, &reply, Kind::MAX_REPLY);
        match event {
            Some(Event::Request(id, body, received)) => match body {
                Request::Ping => send(id, Reply::Pong)?,
                Request::Context {
                    key,
                    ticket,
                    display,
                    exclude,
                } => {
                    if current.is_some_and(|held: Key| held.owner != key.owner) {
                        screen.clear();
                    }
                    live = false;
                    expires = None;
                    let admitted = screen.admit(&ticket, &display, received, now);
                    current = Some(key);
                    let retired = fences
                        .get(&key.owner.thread.0)
                        .is_some_and(|f| key.owner.deletion_generation <= *f);
                    let result =
                        if retired || key.capture != ticket.capture() || capturing.is_some() {
                            Err(Failure::Refused(Refusal::Stale))
                        } else {
                            admitted
                        };
                    match result {
                        Ok(until) => {
                            if let Some(Ok(reader)) = &mut screen.reader {
                                reader.set_shell_pid(ticket.shell_pid());
                            }
                            let job = Job {
                                key,
                                frame: std::mem::take(&mut screen.frame),
                                display,
                                exclude,
                                until,
                            };
                            match native.begin(job) {
                                None => {
                                    live = true;
                                    expires = Some(until);
                                    capturing = Some(Capture {
                                        id,
                                        key,
                                        ticket,
                                        display,
                                        received,
                                        expires: until,
                                    });
                                }
                                Some(mut job) => {
                                    job.frame.release();
                                    send(id, Reply::Busy)?;
                                }
                            }
                        }
                        Err(error) => {
                            screen.clear();
                            send(id, failed(error))?;
                        }
                    }
                }
                Request::Elements { key, target } => {
                    if !live || current != Some(key) || capturing.is_some() || walking.is_some() {
                        send(id, Reply::Refused(Refusal::Stale))?;
                    } else {
                        match screen.walk_job(key.capture, target, now) {
                            Ok(Ok(job)) => {
                                let until = job.until + Duration::from_millis(200);
                                match screen
                                    .reader
                                    .as_mut()
                                    .expect("validated reader")
                                    .as_mut()
                                    .expect("validated reader")
                                    .begin(job)
                                {
                                    Ok(()) => walking = Some((id, key, until)),
                                    Err(walked) => {
                                        send(id, Reply::Elements(key, walked.tree))?;
                                        if walked.retire {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                            Ok(Err(walked)) => {
                                send(id, Reply::Elements(key, walked.tree))?;
                                if walked.retire {
                                    return Ok(());
                                }
                            }
                            Err(Failure::Retire(_)) => return Ok(()),
                            Err(error) => send(id, failed(error))?,
                        }
                    }
                }
                control @ (Request::Drop(_) | Request::Retire(_)) => {
                    let (owner, capture) = match control {
                        Request::Drop(key) => (key.owner, Some(key.capture)),
                        Request::Retire(owner) => {
                            fences
                                .entry(owner.thread.0)
                                .and_modify(|f| *f = (*f).max(owner.deletion_generation))
                                .or_insert(owner.deletion_generation);
                            (owner, None)
                        }
                        _ => unreachable!(),
                    };
                    let matches = current.is_some_and(|held: Key| match capture {
                        Some(capture) => held.owner == owner && held.capture == capture,
                        None => {
                            held.owner.thread == owner.thread
                                && held.owner.deletion_generation <= owner.deletion_generation
                        }
                    });
                    if capture.is_some() && !matches {
                        send(id, Reply::Refused(Refusal::Stale))?;
                    } else {
                        if matches {
                            live = false;
                            expires = None;
                            screen.clear();
                        }
                        if releasing.len() == QUEUE {
                            send(id, Reply::Busy)?;
                        } else {
                            releasing.push((id, owner, capture));
                        }
                    }
                }
            },
            Some(Event::End(result)) => {
                result?;
                return Ok(());
            }
            Some(Event::Wake) | None => {}
        }
        if expires.is_some_and(|until| now() >= until) {
            live = false;
            expires = None;
            screen.clear();
        }
        match native.poll() {
            Ok(Some((key, frame, result))) => {
                screen.frame = frame;
                let pending = capturing.take().expect("one capture owns the result");
                if matches!(result, Err(Failure::Retire(_))) {
                    return Ok(());
                }
                if !live || current != Some(key) || key != pending.key {
                    screen.clear();
                    send(pending.id, Reply::Refused(Refusal::Stale))?;
                } else {
                    let result = result.and_then(|(acquired, foreground)| {
                        screen.acquired(
                            &pending.ticket,
                            &pending.display,
                            pending.received,
                            pending.expires,
                            now(),
                            (acquired, foreground),
                        )
                    });
                    match result {
                        Ok(meta) => {
                            send(pending.id, Reply::Acquired(key, meta))?;
                            send(pending.id, Reply::Complete(key))?;
                        }
                        Err(error) => {
                            live = false;
                            screen.clear();
                            send(pending.id, failed(error))?;
                        }
                    }
                }
            }
            Err(()) => return Ok(()),
            Ok(None) => {}
        }
        if let Some((id, key, _)) = walking {
            if let Some(result) = screen
                .reader
                .as_mut()
                .and_then(|r| r.as_mut().ok())
                .and_then(|r| r.poll())
            {
                walking = None;
                match result {
                    Err(Failure::Retire(_)) => return Ok(()),
                    Ok(walked) if walked.retire => return Ok(()),
                    Ok(walked) if live && current == Some(key) => {
                        send(id, Reply::Elements(key, walked.tree))?;
                    }
                    _ => send(id, Reply::Refused(Refusal::Stale))?,
                }
            } else if walking.is_some_and(|(_, _, until)| now() >= until) {
                return Ok(());
            }
        }
        if capturing.is_none() && walking.is_none() {
            for (id, owner, capture) in releasing.drain(..) {
                send(id, Reply::Released(owner, capture))?;
            }
        }
    }
}

fn failed(error: Failure) -> Reply {
    match error {
        Failure::Refused(why) => Reply::Refused(why),
        Failure::Fault(code) | Failure::Retire(code) => Reply::Fault(code),
    }
}

#[cfg(test)]
mod tests;
