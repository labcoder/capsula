//! Brain-side capture and release of the screen worker's single held frame.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, TypedDisplay};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::actor::{Key, Owner, Reply, Request};
use super::context::ScreenContext;
use super::wire::{CaptureId, MAX_REPLY_BYTES, Refusal, TicketWire};
use crate::platform::Job;
use crate::supervise::{Generation, Handle, Kind, ReplyStream, WorkerLost, Zeroed};

#[cfg(test)]
mod tests;

/// The longest a ticket lets the worker hold a frame: the screen budget [guess]. With no `Retire`
/// (a panic, an aborted task, a dropped `context`), the worker's own expiry clears the frame by then.
pub(crate) const MAX_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

/// The hold protocol: the screen worker's argument and caps, the actor's messages.
pub(crate) struct Holds;

impl Kind for Holds {
    const ARG: &'static str = crate::SCREEN_WORKER;
    const NAME: &'static str = "screen";
    /// Requests are a tag and a hold's key, ticket and point at most.
    const MAX_REQUEST: usize = 64 * 1024;
    const MAX_REPLY: usize = MAX_REPLY_BYTES;
    type Request = Request;
    type Reply = Reply;
    type Assembly = Zeroed;

    /// A capture answers its frame's metadata, then its completion.
    fn is_terminal(reply: &Reply) -> bool {
        !matches!(reply, Reply::Acquired(..))
    }
}

/// A frame pinned to its capture ID and the worker that acquired it.
#[derive(Debug)]
pub(crate) struct Held {
    pub(crate) key: Key,
    pub(crate) generation: Generation,
}

/// A typed frame awaiting the caller's matching, live exclusion proof.
#[derive(Debug)]
pub(crate) struct TypedHeld {
    held: Held,
    cleanup: TypedDrop,
    until: Instant,
}

impl TypedHeld {
    /// The capture named in the shell's exclusion confirmation.
    pub(crate) fn capture_id(&self) -> CaptureId {
        self.held.key.capture
    }

    /// Only call after validating the shell's proof for this capture and lease.
    /// Cancellation or a rejected proof must drop the held frame instead.
    ///
    /// # Errors
    /// The frame expired, changed, was revoked or did not seal on its worker.
    pub(crate) async fn seal(mut self) -> Result<ScreenContext, Closed> {
        if Instant::now() >= self.until {
            return Err(Closed::Refused(Refusal::Expired));
        }
        let context = seal_held(&self.cleanup.inner, &self.held, self.until).await?;
        self.cleanup.armed = false;
        if Instant::now() >= self.until {
            return Err(Closed::Refused(Refusal::Expired));
        }
        Ok(context)
    }
}

#[derive(Debug)]
struct TypedDrop {
    inner: Handle<Holds>,
    key: Key,
    runtime: tokio::runtime::Handle,
    armed: bool,
}

impl Drop for TypedDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let inner = self.inner.clone();
        let key = self.key;
        drop(self.runtime.spawn(async move {
            let until = Instant::now() + MAX_TTL;
            match inner.request(Request::Drop(key), until).await {
                Ok((Reply::Revoked(revoked), _)) if revoked == key => {}
                Ok((Reply::Refused(Refusal::Stale), _)) => {}
                _ => {
                    tracing::debug!("exact screen revocation unavailable; frame expires in worker");
                }
            }
        }));
    }
}

/// Why a capture's screen work ended without what it asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Closed {
    /// The worker refused, without a fault.
    Refused(Refusal),
    /// The worker's capture slot is taken.
    Busy,
    /// A numeric platform fault.
    Fault(i32),
    /// No answer that belongs to this hold: a loss, the deadline, a full queue, or a reply from
    /// another worker or for another frame.
    Lost(WorkerLost),
}

/// A retired hold's release wasn't shown: neither the worker's answer nor its process's exit
/// came in time, which includes a holder that had already exited before `retire` began. It's
/// never fatal: the worker's own ticket expiry clears the frame within [`MAX_TTL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unreleased;

/// One brain's client for the hold protocol and its one screen worker. Each capture gets a fresh
/// capture id and a ticket number above every earlier one, since a worker refuses a number it has
/// already spent. Dropping the last one stops the worker.
#[derive(Debug)]
pub struct HoldClient {
    inner: Handle<Holds>,
    admission: Mutex<Admission>,
}

#[derive(Debug, Default)]
struct Admission {
    last_capture: u64,
    /// Keeps retired holds from reaching replacement workers, which start with no fence.
    hold_fence: Option<Owner>,
}

impl HoldClient {
    /// Starts the screen worker in the brain's job.
    ///
    /// # Errors
    /// A process or thread creation failure.
    pub fn start(exe: PathBuf, jobs: Arc<Job>) -> std::io::Result<Self> {
        Ok(Self::new(crate::supervise::start::<Holds>(exe, jobs)?))
    }

    pub(crate) fn new(inner: Handle<Holds>) -> Self {
        Self {
            inner,
            admission: Mutex::default(),
        }
    }

    /// Whether a worker answers by `until`. Fixtures only.
    ///
    /// # Errors
    /// `Lost` for a loss, the deadline, backoff or any answer but `Pong`.
    #[cfg(debug_assertions)]
    pub async fn ping(&self, until: Instant) -> Result<(), WorkerLost> {
        match self.inner.request(Request::Ping, until).await? {
            (Reply::Pong, _) => Ok(()),
            _ => Err(WorkerLost::Lost),
        }
    }

    /// Makes the worker fail once, as `fault` says. Fixtures only.
    ///
    /// # Errors
    /// The loss that `fault` causes, or the deadline.
    #[cfg(debug_assertions)]
    pub async fn fault(&self, fault: super::WorkerFault, until: Instant) -> Result<(), WorkerLost> {
        self.inner
            .request(Request::Fault(fault), until)
            .await
            .map(drop)
    }

    /// Each worker lost, by generation. Subscribe before the work whose loss it should see.
    #[cfg(debug_assertions)]
    pub fn lost(&self) -> tokio::sync::broadcast::Receiver<Generation> {
        self.inner.lost()
    }

    /// Captures the window under `point` for `owner`, under a ticket that runs out at `until`
    /// or [`MAX_TTL`] after the worker receives it, whichever is sooner.
    ///
    /// # Errors
    /// `Refused(Stale)` for a retired hold, without asking the worker; otherwise the worker's
    /// answer, or `Lost` when no answer that belongs to this capture came by `until`.
    pub(crate) async fn context(
        &self,
        owner: Owner,
        point: PhysicalPoint,
        until: Instant,
    ) -> Result<Held, Closed> {
        self.capture(owner, until, |key, ticket| Request::Context {
            key,
            ticket,
            point,
        })
        .await
    }

    /// Captures the display nearest `point` for `owner`, drawn after `excluded`, the shell's
    /// exclusion stamp, with `ink` to paint when it's sealed, under a ticket as for [`context`].
    ///
    /// # Errors
    /// As for [`context`].
    ///
    /// [`context`]: Self::context
    pub(crate) async fn display(
        &self,
        owner: Owner,
        point: PhysicalPoint,
        excluded: u64,
        ink: PointerPath,
        until: Instant,
    ) -> Result<Held, Closed> {
        self.capture(owner, until, |key, ticket| Request::Display {
            key,
            ticket,
            point,
            excluded,
            ink,
        })
        .await
    }

    /// Captures only the expected display and never carries ink.
    ///
    /// # Errors
    /// A changed or unknown display, deadline, busy slot or worker loss refuses the capture.
    pub(crate) async fn typed_display(
        &self,
        point: PhysicalPoint,
        expected: TypedDisplay,
        excluded: u64,
        until: Instant,
    ) -> Result<TypedHeld, Closed> {
        let now = Instant::now();
        if until <= now {
            return Err(Closed::Refused(Refusal::Expired));
        }
        let until = until.min(now + MAX_TTL);
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| Closed::Lost(WorkerLost::Lost))?;
        let (key, stream) = self.begin_capture(None, until, |_, ticket| Request::TypedDisplay {
            ticket,
            point,
            expected,
            excluded,
        })?;
        let cleanup = TypedDrop {
            inner: self.inner.clone(),
            key,
            runtime,
            armed: true,
        };
        let held = receive_capture(key, stream, Some(expected)).await?;
        if Instant::now() >= until {
            return Err(Closed::Refused(Refusal::Expired));
        }
        Ok(TypedHeld {
            held,
            cleanup,
            until,
        })
    }

    async fn capture(
        &self,
        owner: Owner,
        until: Instant,
        request: impl FnOnce(Key, TicketWire) -> Request,
    ) -> Result<Held, Closed> {
        let (key, stream) = self.begin_capture(Some(owner), until, request)?;
        receive_capture(key, stream, None).await
    }

    fn begin_capture(
        &self,
        owner: Option<Owner>,
        until: Instant,
        request: impl FnOnce(Key, TicketWire) -> Request,
    ) -> Result<(Key, ReplyStream<Holds>), Closed> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| Closed::Fault(error.raw_os_error().unwrap_or(-1)))?;
        let ttl = until.saturating_duration_since(Instant::now()).min(MAX_TTL);
        let ttl_ms = u32::try_from(ttl.as_millis()).unwrap_or(u32::MAX);
        // 0, which no worker reads as later than now, when the counter can't be read.
        let deadline_qpc = crate::platform::qpc_at(until.into_std()).unwrap_or(0);
        let mut admission = self
            .admission
            .lock()
            .map_err(|_| Closed::Lost(WorkerLost::Lost))?;
        if owner
            .zip(admission.hold_fence)
            .is_some_and(|(owner, fence)| owner <= fence)
        {
            return Err(Closed::Refused(Refusal::Stale));
        }
        admission.last_capture = admission
            .last_capture
            .checked_add(1)
            .ok_or(Closed::Lost(WorkerLost::Lost))?;
        // Mint and enqueue under the same lock so both paths reach the worker in ticket order.
        let key = Key {
            owner,
            capture: CaptureId(admission.last_capture),
        };
        let ticket = TicketWire::new(key.capture, key.capture.0, nonce, ttl_ms, deadline_qpc)
            .for_shell(crate::platform::connected_shell_pid());
        let stream = self
            .inner
            .request_stream(request(key, ticket), until)
            .map_err(Closed::Lost)?;
        Ok((key, stream))
    }

    /// The held frame's one image, as captured. The worker releases the frame whatever it answers.
    ///
    /// # Errors
    /// `TooLarge`, `WindowChanged`, `Stale` or another refusal, a fault, or `Lost`.
    pub(crate) async fn seal(&self, held: Held, until: Instant) -> Result<ScreenContext, Closed> {
        seal_held(&self.inner, &held, until).await
    }

    /// Retires `owner` and every earlier hold, here and in the worker, and waits until their
    /// frames are released. `holder` is the worker that captured for `owner`, when one answered.
    /// A worker that answers anything but the release, or can't be reached, may still hold a
    /// frame, so it's ended instead and its exit is the release.
    ///
    /// # Errors
    /// `Unreleased` when neither the release nor that exit was shown by `until`.
    pub(crate) async fn retire(
        &self,
        owner: Owner,
        holder: Option<Generation>,
        until: Instant,
    ) -> Result<(), Unreleased> {
        {
            let mut admission = self.admission.lock().map_err(|_| Unreleased)?;
            admission.hold_fence = admission.hold_fence.max(Some(owner));
        }
        let mut exits = self.inner.exits();
        let worker = match self.inner.request(Request::Retire(owner), until).await {
            // A later worker's answer means the holder's exit was seen.
            Ok((Reply::Released(released, None), from))
                if released == owner && holder.is_none_or(|holder| from >= holder) =>
            {
                return Ok(());
            }
            // A later worker's answer means the holder's exit was seen, so nothing is held.
            Ok((_, from)) if holder.is_some_and(|holder| from > holder) => return Ok(()),
            Ok((_, from)) => from,
            Err(_) => holder.ok_or(Unreleased)?,
        };
        self.inner
            .retirement()
            .request(worker)
            .map_err(|_| Unreleased)?;
        loop {
            match tokio::time::timeout_at(until, exits.recv()).await {
                Ok(Ok(exit)) if exit.generation == worker => {
                    return if exit.confirmed {
                        Ok(())
                    } else {
                        Err(Unreleased)
                    };
                }
                Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
                Ok(Err(RecvError::Closed)) | Err(_) => return Err(Unreleased),
            }
        }
    }
}

async fn seal_held(
    inner: &Handle<Holds>,
    held: &Held,
    until: Instant,
) -> Result<ScreenContext, Closed> {
    match inner.request(Request::Seal(held.key), until).await {
        Ok((Reply::Sealed(key, context), from)) if key == held.key && from == held.generation => {
            Ok(context)
        }
        Ok((reply, from)) if from == held.generation => Err(closed(&reply)),
        Ok(_) => Err(Closed::Lost(WorkerLost::Lost)),
        Err(lost) => Err(Closed::Lost(lost)),
    }
}

async fn receive_capture(
    key: Key,
    mut stream: ReplyStream<Holds>,
    expected: Option<TypedDisplay>,
) -> Result<Held, Closed> {
    let generation = match stream.next().await {
        Some(Ok((Reply::Acquired(acquired, meta), generation)))
            if acquired == key && meta.capture == key.capture =>
        {
            if expected
                .is_some_and(|wanted| wanted.bounds != meta.display || wanted.dpi != meta.dpi)
            {
                return Err(Closed::Refused(Refusal::DisplayChanged));
            }
            generation
        }
        Some(Ok((reply, _))) => return Err(closed(&reply)),
        Some(Err(lost)) => return Err(Closed::Lost(lost)),
        None => return Err(Closed::Lost(WorkerLost::Lost)),
    };
    match stream.next().await {
        Some(Ok((Reply::Complete(complete), from))) if complete == key && from == generation => {
            Ok(Held { key, generation })
        }
        Some(Err(lost)) => Err(Closed::Lost(lost)),
        _ => Err(Closed::Lost(WorkerLost::Lost)),
    }
}

/// Answers the worker's next request, which must retire `key`'s hold, as released.
#[cfg(test)]
pub(crate) async fn release_next(
    rig: &mut crate::supervise::fixture::Rig<Holds>,
    key: crate::coordinator::HoldKey,
) {
    let owner = Owner::from(key);
    let (id, request) = rig.next().await;
    assert!(matches!(request, Request::Retire(asked) if asked == owner));
    rig.reply(id, &Reply::Released(owner, None)).await;
}

/// Answers the capture `request`, numbered `id`, with a frame this worker holds.
#[cfg(test)]
pub(crate) async fn hold_frame(
    rig: &crate::supervise::fixture::Rig<Holds>,
    id: u64,
    request: Request,
) {
    let (key, display, dpi) = match request {
        Request::Context { key, .. } | Request::Display { key, .. } => (
            key,
            kumochi_protocol::geometry::PhysicalRect {
                origin: [0, 0],
                size: [4, 3],
            },
            96,
        ),
        Request::TypedDisplay {
            ticket, expected, ..
        } => (
            Key {
                owner: None,
                capture: ticket.capture(),
            },
            expected.bounds,
            expected.dpi,
        ),
        _ => panic!("not a capture: {request:?}"),
    };
    let meta = super::wire::FrameMeta {
        capture: key.capture,
        display,
        dpi,
        acquired_after_ms: 1,
    };
    rig.reply(id, &Reply::Acquired(key, meta)).await;
    rig.reply(id, &Reply::Complete(key)).await;
}

/// A terminal answer that isn't the one asked for. The caller drops it, and with it anything it
/// carries: an image is zeroed on drop.
fn closed(reply: &Reply) -> Closed {
    match reply {
        Reply::Refused(refusal) => Closed::Refused(*refusal),
        Reply::Busy => Closed::Busy,
        Reply::Fault(code) => Closed::Fault(*code),
        _ => Closed::Lost(WorkerLost::Lost),
    }
}
