//! The screen worker's loop: it owns one hold's frame from its capture through its walk to its
//! image, or its release.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use kumochi_protocol::frame;
use kumochi_protocol::geometry::PhysicalPoint;
use serde::{Deserialize, Serialize};

use super::capture::{Capturer, Failure, Screen, Target};
use super::client::Holds as Kind;
use super::context::ScreenContext;
use super::elements::{ElementTree, OpenElements, Wake};
use super::encode::JPEG_CAP;
use super::mask::Completeness;
use super::native::{CaptureThread, Job};
use super::reader::{CONNECTION_TIMEOUT, TRANSACTION_TIMEOUT};
#[cfg(debug_assertions)]
use super::wire::WorkerFault;
use super::wire::{CaptureId, ElementTarget, FrameMeta, Refusal, TicketWire};
use crate::supervise::{Kind as _, read_request, write_sized};

const QUEUE: usize = 16;
const _: () = assert!(
    JPEG_CAP + 4096 <= super::wire::MAX_REPLY_BYTES,
    "a sealed reply, the JPEG plus well under 4 KiB, must fit the reply cap"
);

/// A hold: its shell connection's epoch, then the gesture within it. The order is the brain's
/// own, so one high-water fence retires every earlier hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub(super) epoch: u64,
    pub(super) gesture: u64,
}

impl From<crate::coordinator::HoldKey> for Owner {
    fn from(key: crate::coordinator::HoldKey) -> Self {
        Self {
            epoch: key.epoch,
            gesture: u64::from(key.gesture.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Key {
    pub(super) owner: Owner,
    pub(super) capture: CaptureId,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Ping,
    /// One frame of the window under `point`, the hold's pointer when it was recognized.
    Context {
        key: Key,
        ticket: TicketWire,
        point: PhysicalPoint,
    },
    Elements {
        key: Key,
        target: ElementTarget,
    },
    /// The hold's one walk of the captured window, kept in the worker for the seal's masks; only
    /// how complete they are comes back.
    Walk {
        key: Key,
    },
    /// The held frame's one image, after its walk; the frame is released whatever the answer.
    Seal(Key),
    Drop(Key),
    Retire(Owner),
    /// Trigger one failure without modifying the parent's environment.
    #[cfg(debug_assertions)]
    Fault(WorkerFault),
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reply {
    Pong,
    Acquired(Key, FrameMeta),
    Complete(Key),
    Elements(Key, ElementTree),
    Walked(Key, Completeness),
    Sealed(Key, ScreenContext),
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
    received: Instant,
    expires: Instant,
}

/// Serves stdin/stdout after the caller initializes content-free logging. Replies go to an
/// unbuffered stdout, since they can carry window text and images.
///
/// # Errors
/// Returns a framing or transport failure, never request contents.
pub fn serve() -> Result<(), frame::FrameError> {
    serve_on(
        std::io::stdin(),
        &mut crate::platform::raw_stdout()?,
        open,
        open_elements,
        Instant::now,
        crate::platform::window_unchanged,
        crate::platform::qpc_left,
    )
}

pub(super) fn serve_on(
    mut input: impl std::io::Read + Send + 'static,
    output: &mut impl std::io::Write,
    open: impl FnMut() -> Result<Box<dyn Capturer>, Failure> + Send + 'static,
    elements: OpenElements,
    now: impl Fn() -> Instant + Send + Sync + 'static,
    unchanged: fn(&Target) -> bool,
    left_until: fn(u64) -> Option<Duration>,
) -> Result<(), frame::FrameError> {
    #[cfg(debug_assertions)]
    let mut fault = std::env::var(kumochi_protocol::constants::SCREEN_FAULT_ENV)
        .ok()
        .and_then(|value| match value.as_str() {
            "hang" => Some(WorkerFault::Hang),
            "crash" => Some(WorkerFault::Crash),
            "garbage" => Some(WorkerFault::Garbage),
            "panic" => Some(WorkerFault::Panic),
            "exit" => Some(WorkerFault::Exit),
            _ => None,
        });
    #[cfg(debug_assertions)]
    if fault == Some(WorkerFault::Exit) {
        return Ok(());
    }
    let (inbox, events) = mpsc::sync_channel(QUEUE);
    let capture = CaptureThread::open(open, wake(inbox.clone()))
        .map_err(|_| std::io::Error::other("capture thread"))?;
    let mut screen = Screen::default();
    screen.unchanged = unchanged;
    screen.left_until = left_until;
    screen.reader = Some(elements(wake(inbox.clone())));
    let now = std::sync::Arc::new(now);
    let clock = std::sync::Arc::clone(&now);
    drop(std::thread::spawn(move || {
        let ended = loop {
            match read_request::<Request>(&mut input, Kind::MAX_REQUEST) {
                Ok(Some((id, body))) => {
                    #[cfg(debug_assertions)]
                    let body = fault.take().map_or(body, Request::Fault);
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
    let mut fence: Option<Owner> = None;
    let mut current = None;
    let mut live = false;
    let mut expires = None;
    let mut capturing: Option<Capture> = None;
    let mut walking: Option<(u64, Key, Instant, Answer)> = None;
    let mut releasing = Vec::new();
    loop {
        let deadline = expires
            .into_iter()
            .chain(walking.map(|(_, _, until, _)| until))
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
        #[cfg(debug_assertions)]
        let event = match event {
            Some(Event::Request(_, Request::Fault(fault), _)) => {
                if trigger(fault, output)? {
                    return Ok(());
                }
                None
            }
            event => event,
        };
        let mut send = |id, reply| write_sized(output, id, &reply, Kind::MAX_REPLY);
        match event {
            Some(Event::Request(id, body, received)) => {
                let walk = match body {
                    Request::Ping => {
                        send(id, Reply::Pong)?;
                        None
                    }
                    Request::Context { key, ticket, point } => {
                        if current.is_some_and(|held: Key| held.owner != key.owner) {
                            screen.clear();
                        }
                        live = false;
                        expires = None;
                        let admitted = screen.admit(&ticket, received, now);
                        current = Some(key);
                        let retired = fence.is_some_and(|fence| key.owner <= fence);
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
                                    point,
                                    shell: ticket.shell_pid(),
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
                        None
                    }
                    Request::Elements { key, target } => Some((key, target, Answer::Tree)),
                    Request::Walk { key } => Some((key, ElementTarget::Foreground, Answer::Masks)),
                    #[cfg(debug_assertions)]
                    Request::Fault(_) => None,
                    Request::Seal(key) => {
                        if !live || current != Some(key) || capturing.is_some() || walking.is_some()
                        {
                            send(id, Reply::Refused(Refusal::Stale))?;
                        } else {
                            live = false;
                            expires = None;
                            match screen.seal(key.capture, JPEG_CAP) {
                                Ok(context) => send(id, Reply::Sealed(key, context))?,
                                Err(error) => send(id, failed(error))?,
                            }
                        }
                        None
                    }
                    control @ (Request::Drop(_) | Request::Retire(_)) => {
                        let (owner, capture) = match control {
                            Request::Drop(key) => (key.owner, Some(key.capture)),
                            Request::Retire(owner) => {
                                fence = fence.max(Some(owner));
                                (owner, None)
                            }
                            _ => unreachable!(),
                        };
                        let matches = current.is_some_and(|held: Key| match capture {
                            Some(capture) => held.owner == owner && held.capture == capture,
                            None => held.owner <= owner,
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
                        None
                    }
                };
                if let Some((key, target, summary)) = walk {
                    if !live || current != Some(key) || capturing.is_some() || walking.is_some() {
                        send(id, Reply::Refused(Refusal::Stale))?;
                    } else {
                        match screen.walk_job(key.capture, target, now) {
                            Ok(Ok(job)) => {
                                let until = job.until + CONNECTION_TIMEOUT + TRANSACTION_TIMEOUT;
                                match screen
                                    .reader
                                    .as_mut()
                                    .expect("validated reader")
                                    .as_mut()
                                    .expect("validated reader")
                                    .begin(job)
                                {
                                    Ok(()) => walking = Some((id, key, until, summary)),
                                    Err(walked) => {
                                        send(id, answer(summary, key, walked.tree, &screen))?;
                                        if walked.retire {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                            Ok(Err(walked)) => {
                                send(id, answer(summary, key, walked.tree, &screen))?;
                                if walked.retire {
                                    return Ok(());
                                }
                            }
                            Err(Failure::Retire(_)) => return Ok(()),
                            Err(error) => send(id, failed(error))?,
                        }
                    }
                }
            }
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
                    let result = result.and_then(|(acquired, target)| {
                        let meta = screen.acquired(
                            &pending.ticket,
                            target.top.rect,
                            pending.received,
                            pending.expires,
                            now(),
                            (acquired, Some(target.top.clone())),
                        )?;
                        screen.pin(target);
                        Ok(meta)
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
        if let Some((id, key, _, summary)) = walking {
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
                        send(id, answer(summary, key, walked.tree, &screen))?;
                    }
                    _ => send(id, Reply::Refused(Refusal::Stale))?,
                }
            } else if walking.is_some_and(|(_, _, until, _)| now() >= until) {
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

/// What a walk answers: its tree for `Elements`; for `Walk`, only how complete its masks are, so
/// no window text leaves the worker.
#[derive(Clone, Copy)]
enum Answer {
    Tree,
    Masks,
}

fn answer(summary: Answer, key: Key, tree: ElementTree, screen: &Screen) -> Reply {
    match summary {
        Answer::Masks => Reply::Walked(key, screen.completeness()),
        Answer::Tree => Reply::Elements(key, tree),
    }
}

/// Triggers one dev-only failure; `Ok(true)` ends the worker.
#[cfg(debug_assertions)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "explicit dev-only panic-hook fixture"
)]
fn trigger(
    fault: WorkerFault,
    output: &mut impl std::io::Write,
) -> Result<bool, frame::FrameError> {
    match fault {
        WorkerFault::Hang => loop {
            std::thread::park();
        },
        WorkerFault::Crash => std::process::exit(1),
        WorkerFault::Garbage => {
            output.write_all(&[0xff; 4])?;
            output.flush()?;
            Ok(false)
        }
        WorkerFault::Panic => panic!("private screen fault payload"),
        WorkerFault::Exit => Ok(true),
    }
}

#[cfg(windows)]
fn open() -> Result<Box<dyn Capturer>, Failure> {
    crate::platform::screen_capturer()
}

/// No platform but Windows captures yet, so every capture is refused as `Unsupported`.
// shortcut: macOS will open ScreenCaptureKit here.
#[cfg(not(windows))]
fn open() -> Result<Box<dyn Capturer>, Failure> {
    Err(Refusal::Unsupported.into())
}

/// `E_NOTIMPL`, for a platform with no element reader yet.
#[cfg(not(windows))]
const NO_CAPTURER: i32 = 0x8000_4001_u32.cast_signed();

#[cfg(windows)]
fn open_elements(wake: Wake) -> Result<Box<dyn super::elements::ElementReader>, i32> {
    crate::platform::screen_elements(wake)
}

// shortcut: macOS will open its accessibility source here.
#[cfg(not(windows))]
fn open_elements(_wake: Wake) -> Result<Box<dyn super::elements::ElementReader>, i32> {
    Err(NO_CAPTURER)
}

/// Only `Retire` ends the worker. A refusal or a fault, a window capture's included, answers
/// this request alone: no screen for that hold, and the same capturer serves the next one.
fn failed(error: Failure) -> Reply {
    match error {
        Failure::Refused(why) => Reply::Refused(why),
        Failure::Fault(code) | Failure::Retire(code) => Reply::Fault(code),
    }
}

#[cfg(test)]
mod tests;
