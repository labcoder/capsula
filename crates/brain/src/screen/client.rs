//! The brain's side of a hold's screen work: one frame in the worker, from its capture through its
//! walk to its image, or its release. Dormant: nothing holds a screen part until 2c4, and the
//! worker's entry point still runs the legacy loop.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use kumochi_protocol::geometry::PhysicalPoint;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::actor::{Key, Owner, Reply, Request};
use super::context::ScreenContext;
use super::mask::Completeness;
use super::supervisor::{REQUEST_DEADLINE, Screen};
use super::wire::{CaptureId, FrameMeta, Refusal, TicketWire};
use crate::supervise::{Generation, Handle, Kind, WorkerLost, Zeroed};

#[cfg(test)]
mod tests;

/// The longest a ticket lets the worker hold a frame: the screen budget. With no `Retire` (a
/// panic, an aborted task, a dropped `context`), the worker's own expiry clears the frame by then.
pub(crate) const MAX_TTL: std::time::Duration = REQUEST_DEADLINE;

/// The hold protocol: the screen worker's argument and caps, the actor's messages.
pub(crate) struct Holds;

impl Kind for Holds {
    const ARG: &'static str = Screen::ARG;
    const NAME: &'static str = Screen::NAME;
    const MAX_REQUEST: usize = Screen::MAX_REQUEST;
    const MAX_REPLY: usize = Screen::MAX_REPLY;
    type Request = Request;
    type Reply = Reply;
    type Assembly = Zeroed;

    /// A capture answers its frame's metadata, then its completion.
    fn is_terminal(reply: &Reply) -> bool {
        !matches!(reply, Reply::Acquired(..))
    }
}

/// A frame the worker holds for one hold, on the worker that holds it. A later answer from any
/// other worker is lost: a replacement holds nothing.
#[derive(Debug)]
pub(crate) struct Held {
    pub(crate) key: Key,
    pub(crate) generation: Generation,
    pub(crate) meta: FrameMeta,
}

/// Why a hold's screen work ended without what it asked for.
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

/// One brain's client for the hold protocol. Each capture gets a fresh capture id and a ticket
/// number above every earlier one, since a worker refuses a number it has already spent.
#[derive(Debug)]
pub(crate) struct HoldClient {
    inner: Handle<Holds>,
    tickets: AtomicU64,
    captures: AtomicU64,
    /// Every hold at or below it is retired. The worker's own fence ends with its process, so
    /// this one keeps a retired hold from reaching a replacement.
    fence: Mutex<Option<Owner>>,
}

impl HoldClient {
    pub(crate) fn new(inner: Handle<Holds>) -> Self {
        Self {
            inner,
            tickets: AtomicU64::new(1),
            captures: AtomicU64::new(1),
            fence: Mutex::new(None),
        }
    }

    /// Captures the window under `point` for `owner`, under a ticket that runs out at `until`
    /// or after [`MAX_TTL`], whichever is sooner.
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
        let key = Key {
            owner,
            capture: CaptureId(self.captures.fetch_add(1, Ordering::Relaxed)),
        };
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| Closed::Fault(error.raw_os_error().unwrap_or(-1)))?;
        let ttl = until.saturating_duration_since(Instant::now()).min(MAX_TTL);
        let ttl_ms = u32::try_from(ttl.as_millis()).unwrap_or(u32::MAX);
        let mut stream = {
            let fence = self
                .fence
                .lock()
                .map_err(|_| Closed::Lost(WorkerLost::Lost))?;
            if fence.is_some_and(|fence| owner <= fence) {
                return Err(Closed::Refused(Refusal::Stale));
            }
            // Minted under the lock, so tickets reach the worker in the order of their numbers.
            let ticket = TicketWire::new(
                key.capture,
                self.tickets.fetch_add(1, Ordering::Relaxed),
                nonce,
                ttl_ms,
            )
            .for_shell(crate::platform::connected_shell_pid());
            self.inner
                .request_stream(Request::Context { key, ticket, point }, until)
                .map_err(Closed::Lost)?
        };
        let (meta, generation) = match stream.next().await {
            Some(Ok((Reply::Acquired(acquired, meta), generation)))
                if acquired == key && meta.capture == key.capture =>
            {
                (meta, generation)
            }
            Some(Ok((reply, _))) => return Err(closed(&reply)),
            Some(Err(lost)) => return Err(Closed::Lost(lost)),
            None => return Err(Closed::Lost(WorkerLost::Lost)),
        };
        match stream.next().await {
            Some(Ok((Reply::Complete(complete), from)))
                if complete == key && from == generation =>
            {
                Ok(Held {
                    key,
                    generation,
                    meta,
                })
            }
            Some(Err(lost)) => Err(Closed::Lost(lost)),
            _ => Err(Closed::Lost(WorkerLost::Lost)),
        }
    }

    /// The hold's one walk of the captured window; only how complete its masks are comes back.
    ///
    /// # Errors
    /// The worker's refusal or fault, or `Lost`.
    pub(crate) async fn walk(&self, held: &Held, until: Instant) -> Result<Completeness, Closed> {
        match self
            .answer(held, Request::Walk { key: held.key }, until)
            .await?
        {
            Reply::Walked(key, completeness) if key == held.key => Ok(completeness),
            reply => Err(closed(&reply)),
        }
    }

    /// The held frame's one image. The worker releases the frame whatever it answers.
    ///
    /// # Errors
    /// `Unmasked`, `TooLarge`, `WindowChanged`, `Stale` or another refusal, a fault, or
    /// `Lost`.
    pub(crate) async fn seal(&self, held: Held, until: Instant) -> Result<ScreenContext, Closed> {
        match self.answer(&held, Request::Seal(held.key), until).await? {
            Reply::Sealed(key, context) if key == held.key => Ok(context),
            reply => Err(closed(&reply)),
        }
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
            let mut fence = self.fence.lock().map_err(|_| Unreleased)?;
            *fence = (*fence).max(Some(owner));
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

    /// `request`'s one answer, when the worker that holds `held` sent it.
    async fn answer(&self, held: &Held, request: Request, until: Instant) -> Result<Reply, Closed> {
        match self.inner.request(request, until).await {
            Ok((reply, from)) if from == held.generation => Ok(reply),
            Ok(_) => Err(Closed::Lost(WorkerLost::Lost)),
            Err(lost) => Err(Closed::Lost(lost)),
        }
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
