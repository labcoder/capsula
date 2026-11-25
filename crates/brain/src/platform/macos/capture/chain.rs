//! One display screenshot as a two-phase chain. The content query's callback does all the
//! `ScreenCaptureKit` work and submits the screenshot; only an owned image or a numeric result
//! comes back to the capture thread, as one message once both phases have finished.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::layout::Screen;
use crate::screen::{Deadline, Failure, Refusal};

const LIVE: u8 = 0;
const COMMITTED: u8 = 1;
const STOPPED: u8 = 2;

/// `SCStreamErrorUserDeclined`.
const USER_DECLINED: isize = -3801;
/// The chain didn't show it finished within the grace, so native work may still be running.
pub(super) const UNFINISHED: i32 = -3;

/// What one screenshot asks for: the display, its size in points and pixels, and the shell's
/// process, whose application the filter leaves out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) display: u32,
    pub(super) points: [u32; 2],
    pub(super) size: [u32; 2],
    pub(super) shell: u32,
}

/// How a chain ended without an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ended {
    Refused(Refusal),
    /// A framework error's code; `sck` when its domain is `SCStreamErrorDomain`. No text is kept.
    Code {
        sck: bool,
        code: isize,
    },
    /// The chain saw a stop or expiry and submitted nothing further.
    Stopped,
}

pub(super) type Done<T> = Box<dyn FnOnce(Result<T, Ended>) + Send>;

/// The native side. Callbacks may run on any thread; `Content` and `Shot` never leave theirs.
pub(super) trait Sck: Send + Sync + 'static {
    type Content;
    type Shot;
    type Image: Send;
    /// The active displays as Quartz reports them.
    ///
    /// # Errors
    /// `Layout` when they can't all be read, or a numeric fault.
    fn displays(&self) -> Result<Vec<Screen>, Failure>;
    /// Whether screen capture is granted, asked without prompting.
    fn preflight(&self) -> bool;
    /// Starts the content query; `ready` runs at most once, on the framework's context.
    fn query(&self, ready: Done<Self::Content>);
    /// Builds the filter and configuration for `request` from the content, which it drops.
    ///
    /// # Errors
    /// `DisplayChanged` when the display isn't there at the same size, `Excluded` unless exactly
    /// one application has the shell's process, or a numeric code.
    fn prepare(&self, content: Self::Content, request: &Request) -> Result<Self::Shot, Ended>;
    /// Submits the screenshot and drops the shot; `done` runs at most once, maybe inline.
    fn shoot(&self, shot: Self::Shot, done: Done<Self::Image>);
    /// Draws `image` over all of `frame` after checking it matches `size`.
    ///
    /// # Errors
    /// `DisplayChanged` for another size, or a numeric fault for an image it can't draw.
    fn draw(&self, image: &Self::Image, size: [u32; 2], frame: &mut [u8]) -> Result<(), Failure>;
    /// The capture thread tried to stop the chain at expiry. Tests order their fakes on it.
    fn expired(&self) {}
}

/// The clocks a chain reads: `now` for the callback's deadline checks, and the continuous
/// clock, which keeps counting through sleep, for the grace.
#[derive(Clone, Copy)]
pub(super) struct Clock {
    pub(super) now: fn() -> Instant,
    pub(super) continuous: fn() -> Option<u64>,
    pub(super) grace: Duration,
}

struct Chain<I> {
    phase: AtomicU8,
    deadline: Deadline,
    now: fn() -> Instant,
    slot: Mutex<Slot<I>>,
}

struct Slot<I> {
    left: u8,
    result: Option<Result<I, Ended>>,
    to: Option<SyncSender<Result<I, Ended>>>,
}

impl<I> Chain<I> {
    fn live(&self) -> bool {
        self.deadline.tighten((self.now)()).is_some()
    }

    /// Marks `phases` finished. Whichever finishes last sends the chain's one result, outside
    /// the lock; a receiver that's gone drops it, releasing its image.
    fn finish(&self, phases: u8, result: Option<Result<I, Ended>>) {
        let (extra, last) = {
            let Ok(mut slot) = self.slot.lock() else {
                return;
            };
            let extra = if slot.result.is_none() {
                slot.result = result;
                None
            } else {
                result
            };
            slot.left = slot.left.saturating_sub(phases);
            let last = if slot.left == 0 {
                slot.result.take().zip(slot.to.take())
            } else {
                None
            };
            (extra, last)
        };
        drop(extra);
        if let Some((result, to)) = last {
            let _unsent = to.try_send(result);
        }
    }
}

/// The content callback: the stop check, the filter, then the grant, the stamp and the commit,
/// with no native call between the stamp read and the screenshot.
fn on_content<S: Sck>(
    sck: &S,
    chain: &Arc<Chain<S::Image>>,
    request: &Request,
    content: Result<S::Content, Ended>,
) {
    let submitted = (|| {
        let content = content?;
        if chain.phase.load(Ordering::Acquire) != LIVE || !chain.live() {
            return Err(Ended::Stopped);
        }
        let shot = sck.prepare(content, request)?;
        if !sck.preflight() {
            return Err(Ended::Refused(Refusal::NotPermitted));
        }
        if !chain.live() {
            return Err(Ended::Stopped);
        }
        chain
            .phase
            .compare_exchange(LIVE, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Ended::Stopped)?;
        let shooting = Arc::clone(chain);
        sck.shoot(
            shot,
            Box::new(move |result| shooting.finish(1, Some(result))),
        );
        Ok(())
    })();
    match submitted {
        Ok(()) => chain.finish(1, None),
        Err(ended) => chain.finish(2, Some(Err(ended))),
    }
}

/// Runs one chain and waits for its result until the local deadline. At expiry it stops the
/// chain if nothing is committed yet, then waits the grace for the chain to show it finished.
///
/// # Errors
/// `Expired` for a stopped or late chain, the chain's refusal or code, or `Retire` when it
/// didn't finish within the grace.
pub(super) fn run<S: Sck>(
    sck: &Arc<S>,
    deadline: Deadline,
    request: Request,
    clock: Clock,
) -> Result<S::Image, Failure> {
    let (to, results) = mpsc::sync_channel(1);
    let chain = Arc::new(Chain {
        phase: AtomicU8::new(LIVE),
        deadline,
        now: clock.now,
        slot: Mutex::new(Slot {
            left: 2,
            result: None,
            to: Some(to),
        }),
    });
    let (native, called) = (Arc::clone(sck), Arc::clone(&chain));
    sck.query(Box::new(move |content| {
        on_content(&*native, &called, &request, content);
    }));
    match results.recv_timeout(deadline.until.saturating_duration_since(Instant::now())) {
        Ok(result) => result.map_err(failure),
        Err(RecvTimeoutError::Timeout) => {
            // Losing means a screenshot is in flight; either way the chain's end is awaited.
            let _stopped =
                chain
                    .phase
                    .compare_exchange(LIVE, STOPPED, Ordering::AcqRel, Ordering::Acquire);
            sck.expired();
            match grace(&results, clock) {
                Some(_late) => Err(Refusal::Expired.into()),
                None => Err(Failure::Retire(UNFINISHED)),
            }
        }
        Err(RecvTimeoutError::Disconnected) => Err(Failure::Retire(UNFINISHED)),
    }
}

/// Waits out the grace on the continuous clock; `None` when it ran out or the clock is unreadable.
fn grace<T>(results: &Receiver<T>, clock: Clock) -> Option<T> {
    let start = (clock.continuous)()?;
    loop {
        let spent = Duration::from_nanos((clock.continuous)()?.checked_sub(start)?);
        let left = clock.grace.saturating_sub(spent);
        match results.recv_timeout(left) {
            Ok(result) => return Some(result),
            Err(RecvTimeoutError::Timeout) if !left.is_zero() => {}
            Err(_) => return None,
        }
    }
}

fn failure(ended: Ended) -> Failure {
    match ended {
        Ended::Refused(refusal) => refusal.into(),
        Ended::Stopped => Refusal::Expired.into(),
        Ended::Code {
            sck: true,
            code: USER_DECLINED,
        } => Refusal::NotPermitted.into(),
        Ended::Code { code, .. } => Failure::Fault(i32::try_from(code).unwrap_or(i32::MIN)),
    }
}
