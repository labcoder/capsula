//! A hold's screen part: its task, the events that bring the task's answers back, and the
//! release a waiting part owes the worker when it closes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::LeaseGeneration;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::context::{Screen, ScreenClass};
use crate::coordinator::HoldKey;
use crate::screen::{Closed, Generation, HoldClient, MAX_TTL, Refusal, ScreenContext};

/// How long the shell's exclusion lease keeps Kumochi's windows out of a capture.
// shortcut: a copy of shell-core's `exclusion::machine::LIFETIME`; move both to the protocol when
// either changes.
const EXCLUSION_LEASE: Duration = Duration::from_secs(3);

/// The oldest a recognition can be when its screen part starts: the worker's ticket ends no later
/// than the deadline it carries, `MAX_TTL` from the start, so within the lease, which was taken no
/// earlier than the recognition.
pub(super) const LATEST_START: Duration = EXCLUSION_LEASE.saturating_sub(MAX_TTL);

/// `None` is a voice hold. A screen hold starts at `Waiting`; its run takes what it ends with,
/// and every other closer leaves `Closed` or `Discarded`.
pub(super) enum ScreenPart {
    None,
    /// `generation` is the worker that captured, once one answered; `lease` is the shell's
    /// exclusion lease over the capture. `stop` tells the hold's task that the part closed.
    Waiting {
        generation: Option<Generation>,
        lease: LeaseGeneration,
        stop: Arc<AtomicBool>,
    },
    Ready(ScreenContext),
    Closed(ScreenClass),
    /// A ready image dropped, and so zeroed, because its hold ended without a run, or any part the
    /// user's switch closed: its run gets no image and no line.
    Discarded,
}

impl std::fmt::Debug for ScreenPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Waiting { generation, .. } => f
                .debug_struct("Waiting")
                .field("generation", generation)
                .finish_non_exhaustive(),
            Self::Ready(_) => f.write_str("Ready"),
            Self::Closed(class) => f.debug_tuple("Closed").field(class).finish(),
            Self::Discarded => f.write_str("Discarded"),
        }
    }
}

/// One answer from the hold `key`'s task, from the worker `generation` once one captured.
pub(in crate::runtime) struct ScreenEvent {
    pub(super) key: HoldKey,
    pub(super) generation: Option<Generation>,
    pub(super) outcome: Outcome,
}

/// What the task got: the worker holds the frame, the image, or why there's none.
pub(super) enum Outcome {
    Held,
    Sealed(ScreenContext),
    Closed(ScreenClass),
    /// The worker's system can't capture a window; the part closes `NotReady` and the brain
    /// remembers it until it restarts, across shell reconnects.
    Unsupported,
}

impl std::fmt::Debug for ScreenEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let outcome = match &self.outcome {
            Outcome::Held => "Held",
            Outcome::Sealed(_) => "Sealed",
            Outcome::Closed(_) => "Closed",
            Outcome::Unsupported => "Unsupported",
        };
        f.debug_struct("ScreenEvent")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .field("outcome", &outcome)
            .finish()
    }
}

impl super::Holds {
    /// Starts the pending hold `key`'s screen part on the window under `point`; a hold that
    /// isn't pending, already has a part, or has no worker client gets none.
    pub(super) fn start_screen(
        &mut self,
        key: HoldKey,
        point: PhysicalPoint,
        lease: LeaseGeneration,
        until: Instant,
    ) -> bool {
        let (Some(client), Some(hold)) = (&self.screen, &mut self.pending) else {
            return false;
        };
        if hold.key != key || !matches!(hold.screen, ScreenPart::None) {
            return false;
        }
        let stop = Arc::<AtomicBool>::default();
        hold.screen = ScreenPart::Waiting {
            generation: None,
            lease,
            stop: Arc::clone(&stop),
        };
        drop(tokio::spawn(work(
            Arc::clone(client),
            key,
            point,
            until,
            stop,
            self.screen_events.clone(),
        )));
        true
    }

    /// Closes the pending hold `key`'s unstarted part as `NotReady`: it was recognized too long
    /// ago for its capture to end within the shell's lease.
    pub(super) fn refuse_screen(&mut self, key: HoldKey) {
        if let Some(hold) = self.pending.as_mut().filter(|hold| hold.key == key)
            && matches!(hold.screen, ScreenPart::None)
        {
            hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
        }
    }

    /// Takes a task's answer into the waiting part it belongs to and drops any other: a dropped
    /// image zeroes, and a dropped `Held` retires its frame.
    pub(super) fn screen_event(&mut self, event: ScreenEvent) {
        let ScreenEvent {
            key,
            generation,
            outcome,
        } = event;
        let waiting = self.pending.as_mut().filter(|hold| hold.key == key);
        let Some(hold) = waiting.filter(|hold| matches!(hold.screen, ScreenPart::Waiting { .. }))
        else {
            if matches!(outcome, Outcome::Held) {
                retire(self.screen.as_ref(), key, generation);
            }
            return;
        };
        let ScreenPart::Waiting {
            generation: holder, ..
        } = &mut hold.screen
        else {
            return;
        };
        match outcome {
            Outcome::Held if holder.is_none() => *holder = generation,
            Outcome::Held => retire(self.screen.as_ref(), key, generation),
            _ if *holder != generation => {}
            Outcome::Sealed(context) => hold.screen = ScreenPart::Ready(context),
            Outcome::Closed(class) => hold.screen = ScreenPart::Closed(class),
            Outcome::Unsupported => hold.screen = ScreenPart::Closed(ScreenClass::NotReady),
        }
    }

    /// Closes the pending hold's screen part for a hold that ends without a run.
    pub(super) fn close_screen(&mut self) {
        self.retire_waiting();
        if let Some(hold) = &mut self.pending
            && matches!(hold.screen, ScreenPart::Ready(_))
        {
            hold.screen = ScreenPart::Discarded;
        }
    }

    /// Closes the pending hold's screen part as `class`, or with no line when there's none,
    /// whatever it reached: a waiting frame is retired, and an image dropped and zeroed.
    pub(super) fn close_screen_as(&mut self, class: Option<ScreenClass>) {
        self.retire_waiting();
        if let Some(hold) = &mut self.pending
            && !matches!(hold.screen, ScreenPart::None)
        {
            hold.screen = class.map_or(ScreenPart::Discarded, ScreenPart::Closed);
        }
    }

    /// Closes the pending part when its own lease lapsed before its frame was captured: a frame
    /// taken later can't be said to exclude our windows. Any other lapse isn't the part's.
    pub(super) fn lease_lapsed(&mut self, lapsed: LeaseGeneration) -> bool {
        let ours = self.pending.as_ref().is_some_and(|hold| {
            matches!(
                hold.screen,
                ScreenPart::Waiting { generation: None, lease, .. } if lease == lapsed
            )
        });
        if ours {
            self.retire_waiting();
        }
        ours
    }

    /// What the pending hold's screen part hands its run. A part still waiting closes first.
    pub(super) fn submit_screen(&mut self) -> Option<Screen> {
        self.retire_waiting();
        let hold = self.pending.as_mut()?;
        match std::mem::replace(&mut hold.screen, ScreenPart::None) {
            ScreenPart::Ready(context) => Some(Screen::Image(context.into_jpeg())),
            ScreenPart::Closed(class) => Some(Screen::NotIncluded(class)),
            ScreenPart::None | ScreenPart::Waiting { .. } | ScreenPart::Discarded => None,
        }
    }

    /// A waiting part may hold a frame in the worker, so it closes as `NotReady`, its task is
    /// stopped, and the worker is told to release it.
    fn retire_waiting(&mut self) {
        let Some(hold) = &mut self.pending else {
            return;
        };
        let ScreenPart::Waiting {
            generation, stop, ..
        } = &hold.screen
        else {
            return;
        };
        stop.store(true, Ordering::SeqCst);
        let generation = *generation;
        hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
        retire(self.screen.as_ref(), hold.key, generation);
    }
}

/// The hold's screen work by `until`: capture, then seal. Each answer goes
/// back as an event, and a frame the task can't hand on is retired with the worker that holds it.
async fn work(
    client: Arc<HoldClient>,
    key: HoldKey,
    point: PhysicalPoint,
    until: Instant,
    stop: Arc<AtomicBool>,
    events: mpsc::Sender<ScreenEvent>,
) {
    let report = |generation, outcome| {
        events.send(ScreenEvent {
            key,
            generation,
            outcome,
        })
    };
    let held = match client.context(key.into(), point, until).await {
        Ok(held) => held,
        Err(closed) => {
            if report(None, ending(closed)).await.is_err() {
                tracing::debug!("a held screen answer came after its brain");
            }
            return;
        }
    };
    let generation = Some(held.generation);
    let ended = report(generation, Outcome::Held).await.is_err() || stop.load(Ordering::SeqCst);
    let outcome = if ended {
        release(&client, key, generation).await;
        None
    } else {
        Some(match client.seal(held, until).await {
            Ok(context) => Outcome::Sealed(context),
            Err(closed) => {
                release(&client, key, generation).await;
                ending(closed)
            }
        })
    };
    if let Some(outcome) = outcome
        && report(generation, outcome).await.is_err()
    {
        tracing::debug!("a held screen answer came after its brain");
    }
}

/// Tells the worker to release `key`'s frame in the background; `holder` is the worker that
/// captured it, when one answered.
fn retire(client: Option<&Arc<HoldClient>>, key: HoldKey, holder: Option<Generation>) {
    // No runtime means the brain is shutting down, and its job ends the worker.
    let (Some(client), Ok(runtime)) = (client, tokio::runtime::Handle::try_current()) else {
        return;
    };
    let client = Arc::clone(client);
    drop(runtime.spawn(async move { release(&client, key, holder).await }));
}

async fn release(client: &HoldClient, key: HoldKey, holder: Option<Generation>) {
    if client
        .retire(key.into(), holder, Instant::now() + MAX_TTL)
        .await
        .is_err()
    {
        tracing::debug!("a held screen frame's release wasn't shown");
    }
}

/// How an answer that isn't the one asked for ends the part. A system that can't capture is told
/// apart, so the brain can stop offering screen holds.
pub(super) fn ending(closed: Closed) -> Outcome {
    if closed == Closed::Refused(Refusal::Unsupported) {
        Outcome::Unsupported
    } else {
        Outcome::Closed(class(closed))
    }
}

/// The class of the answer that ends a part. Every refusal is named, so a new one doesn't
/// compile until it has a class.
pub(super) fn class(closed: Closed) -> ScreenClass {
    let refusal = match closed {
        Closed::Busy | Closed::Fault(_) | Closed::Lost(_) => return ScreenClass::NotReady,
        Closed::Refused(refusal) => refusal,
    };
    match refusal {
        Refusal::Expired | Refusal::Unsupported => ScreenClass::NotReady,
        Refusal::Reused => {
            debug_assert!(false, "a hold's ticket or walk was spent twice");
            ScreenClass::NotReady
        }
        Refusal::NoWindow => ScreenClass::NothingThere,
        Refusal::Minimized
        | Refusal::Cloaked
        | Refusal::Elevated
        | Refusal::Protected
        | Refusal::Excluded => ScreenClass::CantShare,
        Refusal::Unmasked | Refusal::Ambiguous => ScreenClass::Unreadable,
        // A window wider or taller than the frame's side cap is refused as `Invalid`.
        Refusal::TooLarge | Refusal::Invalid => ScreenClass::TooLarge,
        Refusal::WindowChanged | Refusal::DisplayChanged | Refusal::Stale => ScreenClass::Changed,
    }
}
