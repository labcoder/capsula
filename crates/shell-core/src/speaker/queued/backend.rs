use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, Thread};
use std::time::Instant;

use crate::platform::speaker_cpal::{BackendStream, Control, OutputError, Transport};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Power {
    Playing,
    Paused,
}

#[derive(Clone)]
pub(super) enum Request {
    Open(u64, Arc<Control>),
    Power(u64, Power),
    Retire(u64),
}

#[expect(
    clippy::large_enum_variant,
    reason = "one bounded reply slot owns the opened transport without an extra allocation"
)]
pub(super) enum Reply {
    Opened(u64, Option<Transport>),
    Powered(u64, bool),
    Retired(u64),
}

pub(super) fn spawn(
    owner: Thread,
    origin: Instant,
) -> io::Result<(SyncSender<Request>, Receiver<Reply>)> {
    spawn_with(owner, origin, BackendStream::open)
}

type OpenResult = Result<(BackendStream, Transport), OutputError>;

static NATIVE: AtomicBool = AtomicBool::new(false);

struct Native;

impl Native {
    fn claim() -> Option<Self> {
        NATIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        NATIVE.store(false, Ordering::Release);
    }
}

fn spawn_with(
    owner: Thread,
    origin: Instant,
    open: impl FnMut(&Arc<Control>, Instant) -> OpenResult + Send + 'static,
) -> io::Result<(SyncSender<Request>, Receiver<Reply>)> {
    let (send, requests) = mpsc::sync_channel(1);
    let (replies, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("speaker-device".into())
        .spawn(move || serve(&requests, &replies, &owner, origin, open))?;
    Ok((send, receive))
}

fn serve(
    requests: &Receiver<Request>,
    replies: &SyncSender<Reply>,
    owner: &Thread,
    origin: Instant,
    mut open: impl FnMut(&Arc<Control>, Instant) -> OpenResult,
) {
    let mut stream: Option<(u64, BackendStream, Native)> = None;
    while let Ok(request) = requests.recv() {
        let reply = match request {
            Request::Open(generation, control) => {
                let transport = if stream.is_none()
                    && let Some(permit) = Native::claim()
                    && let Ok((opened, transport)) = open(&control, origin)
                    && opened.play().is_ok()
                {
                    stream = Some((generation, opened, permit));
                    Some(transport)
                } else {
                    None
                };
                Reply::Opened(generation, transport)
            }
            Request::Power(generation, power) => Reply::Powered(
                generation,
                stream.as_ref().is_some_and(|(current, stream, _)| {
                    *current == generation
                        && match power {
                            Power::Playing => stream.play(),
                            Power::Paused => stream.pause(),
                        }
                        .is_ok()
                }),
            ),
            Request::Retire(generation) => {
                if stream
                    .as_ref()
                    .is_some_and(|(current, _, _)| *current == generation)
                {
                    drop(stream.take());
                }
                Reply::Retired(generation)
            }
        };
        if replies.send(reply).is_err() {
            break;
        }
        owner.unpark();
    }
}

#[cfg(test)]
mod tests;
