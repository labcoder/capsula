//! A hold's screen part: its task, the events that bring the task's answers back, and the
//! release a capturing part owes the worker when it closes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::geometry::{PhysicalPoint, PointerPath};
use kumochi_protocol::message::{
    CaptureFailure, CaptureFor, ConnectionKind, ControlToBrain, ControlToShell, LeaseGeneration,
    ModelChoice, ModelSlug, NoticeRevision, ScreenHolds, ScreenOff,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Kind, NoScreen};
use crate::context::{Screen, ScreenClass};
use crate::coordinator::HoldKey;
use crate::runtime::{Full, Link};
use crate::screen::{
    CaptureDeadline, Closed, Generation, HoldClient, MAX_TTL, Refusal, ScreenContext,
};

/// How long the shell's exclusion lease keeps Kumochi's windows out of a capture.
// shortcut: a copy of shell-core's `exclusion::machine::LIFETIME`; move both to the protocol when
// either changes.
const EXCLUSION_LEASE: Duration = Duration::from_secs(3);

/// The oldest a recognition can be when its screen part starts: the worker's ticket ends no later
/// than the deadline it carries, `MAX_TTL` from the start, so within the lease, which was taken no
/// earlier than the recognition.
pub(super) const LATEST_START: Duration = EXCLUSION_LEASE.saturating_sub(MAX_TTL);

/// `None` is a voice hold. A window hold starts at `Waiting`, a screen hold at `Armed` and then
/// `Capturing` from its release; its run takes what it ends with, and every other closer leaves
/// `Closed` or `Discarded`.
pub(super) enum ScreenPart {
    None,
    /// A screen hold before its release: nothing captured, no lease and no ticket.
    Armed,
    /// A screen hold's capture from its release, as `Waiting`; `until` is its ticket's deadline,
    /// which also bounds how long the hold's words wait for its picture.
    Capturing {
        generation: Option<Generation>,
        lease: LeaseGeneration,
        stop: Arc<AtomicBool>,
        until: Instant,
    },
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
            Self::Armed => f.write_str("Armed"),
            Self::Waiting { generation, .. } => f
                .debug_struct("Waiting")
                .field("generation", generation)
                .finish_non_exhaustive(),
            Self::Capturing { generation, .. } => f
                .debug_struct("Capturing")
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
    pub(super) deadline: CaptureDeadline,
    pub(super) outcome: Outcome,
}

/// What the task got: the worker holds the frame, the image, or why there's none.
pub(super) enum Outcome {
    Held,
    Sealed(ScreenContext),
    /// The worker's answer, or the loss, that ended the task.
    Closed(Closed),
    /// The worker's system can't capture a window; the part closes `NotReady` and the brain
    /// remembers it until it restarts, across shell reconnects.
    Unsupported,
}

#[derive(PartialEq, Eq)]
pub(super) enum ScreenAcceptance {
    None,
    ExpiredHeld,
    Sealed,
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
            .finish_non_exhaustive()
    }
}

/// What a hold's task captures.
pub(super) enum Aim {
    /// The window under the point, for a window hold.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no shell sends a window hold; its worker path stays tested"
        )
    )]
    Window(PhysicalPoint),
    /// The display under `point`, drawn after `excluded`, with `ink` to paint, for a screen hold.
    Display {
        point: PhysicalPoint,
        excluded: u64,
        ink: PointerPath,
    },
}

impl super::Holds {
    /// Starts the pending hold `key`'s screen part on the window under `point`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "no shell sends a window hold; its worker path stays tested"
        )
    )]
    pub(super) fn start_screen(
        &mut self,
        key: HoldKey,
        point: PhysicalPoint,
        lease: LeaseGeneration,
        deadline: CaptureDeadline,
        recipient: (ConnectionKind, ModelSlug),
    ) -> bool {
        if !self.start(key, Aim::Window(point), lease, deadline) {
            return false;
        }
        self.pending
            .as_mut()
            .expect("the started part has a pending hold")
            .screen_recipient = Some(recipient);
        true
    }

    /// Starts the pending hold `key`'s screen part on `aim`: a window from no part, a display
    /// from `Armed`. A hold that isn't pending, has another part, or has no worker client gets
    /// none.
    pub(super) fn start(
        &mut self,
        key: HoldKey,
        aim: Aim,
        lease: LeaseGeneration,
        deadline: CaptureDeadline,
    ) -> bool {
        let (Some(client), Some(hold)) = (&self.screen, &mut self.pending) else {
            return false;
        };
        let from = match aim {
            Aim::Window(_) => matches!(hold.screen, ScreenPart::None),
            Aim::Display { .. } => matches!(hold.screen, ScreenPart::Armed),
        };
        if hold.key != key || !from {
            return false;
        }
        let stop = Arc::<AtomicBool>::default();
        hold.screen = match aim {
            Aim::Window(_) => ScreenPart::Waiting {
                generation: None,
                lease,
                stop: Arc::clone(&stop),
            },
            Aim::Display { .. } => ScreenPart::Capturing {
                generation: None,
                lease,
                stop: Arc::clone(&stop),
                until: deadline.until,
            },
        };
        drop(tokio::spawn(work(
            Arc::clone(client),
            key,
            aim,
            deadline,
            self.screen_time_left,
            stop,
            self.screen_events.clone(),
        )));
        true
    }

    /// Arms the pending hold `key`'s screen part under offer `notice`; false when it isn't
    /// pending or has a part.
    pub(super) fn arm_screen(
        &mut self,
        key: HoldKey,
        recipient: (ConnectionKind, ModelSlug),
        notice: Option<NoticeRevision>,
    ) -> bool {
        let Some(hold) = self.pending.as_mut().filter(|hold| hold.key == key) else {
            return false;
        };
        if !matches!(hold.screen, ScreenPart::None) {
            return false;
        }
        hold.screen = ScreenPart::Armed;
        hold.screen_recipient = Some(recipient);
        hold.armed_notice = notice;
        true
    }

    /// Closes the pending hold `key`'s armed part as `part`, after a release that started
    /// nothing; any other part is left as it is.
    pub(super) fn close_armed(&mut self, key: HoldKey, part: ScreenPart) {
        if let Some(hold) = self.pending.as_mut().filter(|hold| hold.key == key)
            && matches!(hold.screen, ScreenPart::Armed)
        {
            tracing::info!(closed = ?part, "hold screen part closed");
            hold.screen = part;
        }
    }

    /// Takes a task's answer into the capturing part it belongs to and drops any other: a dropped
    /// image zeroes, and a dropped `Held` retires its frame.
    pub(super) fn screen_event(&mut self, event: ScreenEvent) -> ScreenAcceptance {
        let ScreenEvent {
            key,
            generation,
            deadline,
            outcome,
        } = event;
        let waiting = self.pending.as_mut().filter(|hold| hold.key == key);
        let Some(hold) = waiting.filter(|hold| capturing(&hold.screen)) else {
            if matches!(outcome, Outcome::Held) {
                retire(self.screen.as_ref(), key, generation);
            }
            return ScreenAcceptance::None;
        };
        let (ScreenPart::Waiting {
            generation: holder,
            stop,
            ..
        }
        | ScreenPart::Capturing {
            generation: holder,
            stop,
            ..
        }) = &mut hold.screen
        else {
            return ScreenAcceptance::None;
        };
        let expired = Closed::Refused(Refusal::Expired);
        match outcome {
            Outcome::Held if holder.is_none() && deadline.is_live(self.screen_time_left) => {
                *holder = generation;
            }
            Outcome::Held if holder.is_none() => {
                stop.store(true, Ordering::SeqCst);
                hold.screen = ScreenPart::Closed(class(expired));
                retire(self.screen.as_ref(), key, generation);
                return ScreenAcceptance::ExpiredHeld;
            }
            Outcome::Held => {}
            _ if *holder != generation => {}
            Outcome::Sealed(context) if deadline.is_live(self.screen_time_left) => {
                tracing::info!("hold screen part sealed");
                hold.screen = ScreenPart::Ready(context);
                return ScreenAcceptance::Sealed;
            }
            Outcome::Sealed(_) | Outcome::Closed(Closed::Refused(Refusal::Expired)) => {
                tracing::info!(closed = ?expired, "hold screen part closed");
                hold.screen = ScreenPart::Closed(class(expired));
                if generation.is_some() {
                    retire(self.screen.as_ref(), key, generation);
                }
            }
            Outcome::Closed(closed) => {
                tracing::info!(?closed, "hold screen part closed");
                hold.screen = ScreenPart::Closed(class(closed));
            }
            Outcome::Unsupported => {
                let closed = Closed::Refused(Refusal::Unsupported);
                tracing::info!(?closed, "hold screen part closed");
                hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
            }
        }
        ScreenAcceptance::None
    }

    /// Closes the pending hold's screen part for a hold that ends without a run.
    pub(super) fn close_screen(&mut self) {
        self.retire_waiting();
        if let Some(hold) = &mut self.pending
            && matches!(hold.screen, ScreenPart::Ready(_))
        {
            tracing::info!(closed = "hold ended", "hold screen part closed");
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
            if matches!(hold.screen, ScreenPart::Ready(_)) {
                tracing::info!(closed = ?class, "hold screen part closed");
            }
            hold.screen = class.map_or(ScreenPart::Discarded, ScreenPart::Closed);
        }
    }

    /// Closes the pending part when its own lease lapsed before its frame was captured: a frame
    /// taken later can't be said to exclude our windows. Any other lapse isn't the part's.
    pub(super) fn lease_lapsed(&mut self, lapsed: LeaseGeneration) -> bool {
        let ours = self.pending.as_ref().is_some_and(|hold| {
            matches!(
                hold.screen,
                ScreenPart::Waiting { generation: None, lease, .. }
                | ScreenPart::Capturing { generation: None, lease, .. } if lease == lapsed
            )
        });
        if ours {
            self.retire_waiting();
        }
        ours
    }

    /// What the pending hold's screen part hands its run, under a screen hold's own lines for a
    /// screen hold. A part still capturing closes first, and an armed one is `NotReady`.
    pub(super) fn submit_screen(&mut self) -> Option<Screen> {
        self.retire_waiting();
        let hold = self.pending.as_mut()?;
        let choice = hold
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.defaults.choice.as_ref());
        if matches!(hold.screen, ScreenPart::Ready(_)) && !hold.matches_screen_recipient(choice) {
            tracing::info!(closed = ?ScreenClass::NotReady, "hold screen part closed");
            hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
        }
        let picture = hold.kind == super::Kind::Screen;
        match std::mem::replace(&mut hold.screen, ScreenPart::None) {
            ScreenPart::Ready(context) if picture => {
                let (ink, geometry) = (context.ink(), context.geometry());
                Some(Screen::Picture(context.into_jpeg(), ink, geometry))
            }
            ScreenPart::Ready(context) => Some(Screen::Image(context.into_jpeg())),
            ScreenPart::Closed(class) if picture => Some(Screen::PictureNotIncluded(class)),
            ScreenPart::Closed(class) => Some(Screen::NotIncluded(class)),
            ScreenPart::Armed => Some(Screen::PictureNotIncluded(ScreenClass::NotReady)),
            ScreenPart::None
            | ScreenPart::Waiting { .. }
            | ScreenPart::Capturing { .. }
            | ScreenPart::Discarded => None,
        }
    }

    /// A waiting or capturing part may hold a frame in the worker, so it closes as `NotReady`, its
    /// task is stopped, and the worker is told to release it.
    fn retire_waiting(&mut self) {
        let Some(hold) = &mut self.pending else {
            return;
        };
        let (ScreenPart::Waiting {
            generation, stop, ..
        }
        | ScreenPart::Capturing {
            generation, stop, ..
        }) = &hold.screen
        else {
            return;
        };
        stop.store(true, Ordering::SeqCst);
        let generation = *generation;
        tracing::info!(closed = "still waiting", "hold screen part closed");
        hold.screen = ScreenPart::Closed(ScreenClass::NotReady);
        retire(self.screen.as_ref(), hold.key, generation);
    }
}

impl crate::runtime::Brain {
    /// Arms the admitted screen hold `key` when the gate is open for the default model now, since
    /// the shell's state may lag. It captures nothing until its release.
    pub(super) fn hold_arm(
        &mut self,
        key: HoldKey,
        notice: Option<NoticeRevision>,
    ) -> Result<(), NoScreen> {
        if !self.offer_holds(notice) {
            return Err(NoScreen::Unoffered);
        }
        let choice = self.config.view().default_model.clone();
        if let ScreenHolds::Off(off) = self.display_gate(choice.as_ref()) {
            return Err(NoScreen::Gate(off));
        }
        let choice = choice.ok_or(NoScreen::Gate(ScreenOff::NotReady))?;
        if !self
            .holds
            .arm_screen(key, (choice.connection, choice.model), notice)
        {
            return Err(NoScreen::NotStarted);
        }
        Ok(())
    }

    /// Takes the shell's `ScreenRelease` for the gesture of `key`. A release for a hold whose
    /// capture may still run is dropped unanswered: the answer would end that capture's lease, so
    /// the release's own lease is left to lapse. Any other that starts nothing is answered
    /// `CaptureFailed`, which ends its lease, and closes an armed part.
    pub(super) fn screen_release(
        &mut self,
        key: HoldKey,
        message: &ControlToBrain,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        let ControlToBrain::ScreenRelease {
            lease,
            t_qpc,
            excluded_qpc,
            pointer,
            path,
            notice,
            ..
        } = message
        else {
            return Ok(());
        };
        if self.holds.pending.as_ref().is_some_and(|hold| {
            hold.key == key
                && (hold.kind == Kind::Window
                    || matches!(hold.screen, ScreenPart::Capturing { .. }))
        }) {
            tracing::info!("hold screen release dropped");
            return Ok(());
        }
        // The only bound on a path's length: about 6,500 points fit in a control frame.
        let started = if path.check().is_err() {
            Err(NoScreen::PathTooLong)
        } else {
            self.hold_release(
                key,
                (*lease, *t_qpc, *excluded_qpc),
                pointer.0,
                path.clone(),
                *notice,
            )
        };
        tracing::info!(?started, "hold screen released");
        let Err(refused) = started else {
            return Ok(());
        };
        self.holds.close_armed(key, refused.part());
        link.control(&ControlToShell::CaptureFailed {
            capture_for: CaptureFor::Gesture(key.gesture),
            reason: CaptureFailure::Unavailable,
        })
    }

    /// Starts the armed screen hold `key`'s capture of the display under `point`: the gate again,
    /// then the release stamp's age as for a window hold's recognition, then the exclusion stamp,
    /// which can't be earlier than the release or later than now. The ticket's deadline bounds
    /// the seal too, which the worker runs to its end with nothing else answered meanwhile: an
    /// overrun closes the part `NotReady`, and its retire waits behind the seal.
    fn hold_release(
        &mut self,
        key: HoldKey,
        (lease, t_qpc, excluded): (LeaseGeneration, u64, u64),
        point: PhysicalPoint,
        ink: PointerPath,
        notice: Option<NoticeRevision>,
    ) -> Result<(), NoScreen> {
        let Some(hold) = self.holds.pending.as_ref().filter(|hold| {
            hold.key == key
                && !hold.cancel.load(Ordering::SeqCst)
                && matches!(hold.screen, ScreenPart::Armed)
        }) else {
            return Err(NoScreen::NotArmed);
        };
        let choice = self.config.view().default_model.clone();
        if let ScreenHolds::Off(off) = self.display_gate(choice.as_ref()) {
            return Err(NoScreen::Gate(off));
        }
        // Only the revision it was armed under, and only while that is still the current offer.
        if notice != hold.armed_notice || !self.offer_holds(notice) {
            return Err(NoScreen::Unoffered);
        }
        // A recipient not read yet may be another model's: nothing starts until it is.
        if hold.snapshot.as_ref().is_none_or(|snapshot| {
            !hold.matches_screen_recipient(snapshot.defaults.choice.as_ref())
        }) {
            return Err(NoScreen::NotArmed);
        }
        // Read before the age, so the ticket can only end sooner than the check allows.
        let deadline = CaptureDeadline::after(MAX_TTL);
        if (self.holds.stamp_age)(t_qpc).is_none_or(|age| age > LATEST_START) {
            return Err(NoScreen::TooLate);
        }
        if excluded < t_qpc {
            return Err(NoScreen::ExcludedEarly);
        }
        // A stamp later than now would hold the worker to its deadline for a frame it can't take.
        if (self.holds.stamp_age)(excluded).is_none() {
            return Err(NoScreen::ExcludedLate);
        }
        let aim = Aim::Display {
            point,
            excluded,
            ink,
        };
        if !self.holds.start(key, aim, lease, deadline) {
            return Err(NoScreen::NotStarted);
        }
        Ok(())
    }

    /// Whether a screen hold on `choice` may show the model the screen now, or why not. The
    /// display capture runs on the platforms that capture a window.
    pub(super) fn display_gate(&self, choice: Option<&ModelChoice>) -> ScreenHolds {
        self.picture_gate(choice, self.show_screen_enabled())
    }
}

/// Whether `part` has a capture under way, whose answers it takes.
pub(super) fn capturing(part: &ScreenPart) -> bool {
    matches!(
        part,
        ScreenPart::Waiting { .. } | ScreenPart::Capturing { .. }
    )
}

/// The hold's screen work by its deadline: capture, then seal. Each answer goes
/// back as an event, and a frame the task can't hand on is retired with the worker that holds it.
async fn work(
    client: Arc<HoldClient>,
    key: HoldKey,
    aim: Aim,
    deadline: CaptureDeadline,
    left_until: fn(u64) -> Option<Duration>,
    stop: Arc<AtomicBool>,
    events: mpsc::Sender<ScreenEvent>,
) {
    let report = |generation, outcome| {
        events.send(ScreenEvent {
            key,
            generation,
            deadline,
            outcome,
        })
    };
    let captured = match aim {
        Aim::Window(point) => client.context(key.into(), point, deadline).await,
        Aim::Display {
            point,
            excluded,
            ink,
        } => {
            client
                .display(key.into(), point, excluded, ink, deadline)
                .await
        }
    };
    let held = match captured {
        Ok(held) => held,
        Err(closed) => {
            if report(None, ending(closed)).await.is_err() {
                tracing::debug!("a held screen answer came after its brain");
            }
            return;
        }
    };
    let generation = Some(held.generation);
    if report(generation, Outcome::Held).await.is_err() || stop.load(Ordering::SeqCst) {
        release(&client, key, generation).await;
        return;
    }
    let expired = Closed::Refused(Refusal::Expired);
    let outcome = if deadline.is_live(left_until) {
        match client.seal(held, deadline.until).await {
            Ok(context) => Outcome::Sealed(context),
            Err(closed) => {
                if closed != expired {
                    release(&client, key, generation).await;
                }
                ending(closed)
            }
        }
    } else {
        Outcome::Closed(expired)
    };
    let needs_release = matches!(outcome, Outcome::Closed(closed) if closed == expired);
    if report(generation, outcome).await.is_err() {
        tracing::debug!("a held screen answer came after its brain");
        if needs_release {
            release(&client, key, generation).await;
        }
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
        Outcome::Closed(closed)
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
