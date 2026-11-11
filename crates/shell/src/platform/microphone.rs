//! One microphone generation's collection authority, independent of GPUI delivery.
//!
//! Only an admitted, running generation whose UI receipt was accepted may collect.
//! Native release/reset and link loss revoke it before deferred UI work; deadlines
//! never prove physical release. A later adapter must preserve input-only operation,
//! stable format/device identity, bounded storage and off-UI confirmed teardown.
//!
//! CPAL 0.18.2 was rejected because WASAPI's input xrun path
//! (`src/host/wasapi/stream.rs:825`) calls `try_emit_error`, which acquires a
//! mutex with `try_lock` at `src/host/error_emit.rs:29`. That is nonblocking,
//! but still violates the no-lock contract, including borrowed error paths.
//! CoreAudio input calls the same helper at `src/host/coreaudio/macos/device.rs:781`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{GestureId, HOLD_START_TIMEOUT_MS};

/// The existing 60-second hold limit, independent of sample delivery.
const HOLD_LIMIT: Duration = Duration::from_secs(60);
const ARMED: u64 = 1;
const OPENING: u64 = 2;
const RUNNING: u64 = 3;
const COLLECTING: u64 = 4;
const RELEASED: u64 = 5;
const CANCELLED: u64 = 6;
const RETIRED: u64 = 7;

/// Fixed diagnostics only; foreign errors and driver identifiers never escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MicError {
    #[error("microphone generation is stale")]
    Stale,
    #[error("microphone was cancelled")]
    Cancelled,
    #[error("microphone session ended")]
    SessionLost,
    #[error("microphone hold limit reached")]
    Deadline,
    #[error("microphone is unavailable")]
    Unavailable,
    #[error("microphone format is unsupported")]
    Format,
    #[error("microphone data is discontinuous")]
    Discontinuity,
    #[error("microphone capacity exceeded")]
    Overflow,
    #[error("microphone shutdown is unconfirmed")]
    Unconfirmed,
}

/// Native input closes this atomic fence without borrowing GPUI.
#[derive(Clone, Debug, Default)]
pub(crate) struct CollectionGate(Arc<AtomicU64>);

impl CollectionGate {
    /// Arms a strictly newer nonzero gesture only after the prior join receipt.
    pub(crate) fn arm(&self, gesture: GestureId) -> Result<Generation, MicError> {
        self.arm_at(gesture, Instant::now())
    }

    fn arm_at(&self, gesture: GestureId, at: Instant) -> Result<Generation, MicError> {
        let old = self.0.load(Ordering::Acquire);
        if gesture.0 == 0 || u64::from(gesture.0) <= old >> 3 || old & 7 != 0 && old & 7 != RETIRED
        {
            return Err(MicError::Stale);
        }
        let key = u64::from(gesture.0) << 3;
        self.0
            .compare_exchange(old, key | ARMED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MicError::Stale)?;
        Ok(Generation {
            gate: Arc::clone(&self.0),
            gesture,
            at,
            startup: Arc::new(OnceLock::new()),
        })
    }

    /// Normal release can never downgrade an earlier cancellation.
    pub(crate) fn release(&self, gesture: GestureId) {
        transition(&self.0, gesture, RELEASED);
    }

    /// Cancellation upgrades normal release and is terminal for this generation.
    pub(crate) fn cancel(&self, gesture: GestureId) {
        transition(&self.0, gesture, CANCELLED);
    }

    /// Reset/input-session loss needs no deferred gesture identifier lookup.
    pub(crate) fn cancel_current(&self) {
        let current = self.0.load(Ordering::Acquire);
        if let Some(id) = u32::try_from(current >> 3).ok().filter(|id| *id != 0) {
            self.cancel(GestureId(id));
        }
    }
}

fn transition(gate: &AtomicU64, gesture: GestureId, target: u64) {
    let key = u64::from(gesture.0) << 3;
    let _unchanged = gate.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        (old & 7 != 0 && old >> 3 == key >> 3 && old & 7 < target).then_some(key | target)
    });
}

/// A checked, single-start generation; cloning it cannot grant a second open.
#[derive(Clone, Debug)]
pub(crate) struct Generation {
    gate: Arc<AtomicU64>,
    gesture: GestureId,
    at: Instant,
    startup: Arc<OnceLock<Instant>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Preparing,
    Collecting,
    Release,
}

impl Generation {
    /// Retires denied admission only if no source start has claimed this generation.
    pub(crate) fn discard_unstarted(&self) -> Result<(), MicError> {
        self.startup
            .set(Instant::now())
            .map_err(|_| MicError::Stale)?;
        self.cancel();
        transition(&self.gate, self.gesture, RETIRED);
        Ok(())
    }

    pub(crate) fn gesture(&self) -> GestureId {
        self.gesture
    }

    fn advance(&self, from: u64, to: u64) -> Result<(), MicError> {
        let key = u64::from(self.gesture.0) << 3;
        self.gate
            .compare_exchange(key | from, key | to, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| MicError::Stale)
    }

    pub(super) fn cancel(&self) {
        transition(&self.gate, self.gesture, CANCELLED);
    }

    pub(super) fn collecting(&self, session: &AtomicBool) -> bool {
        session.load(Ordering::Acquire)
            && self.gate.load(Ordering::Acquire) == (u64::from(self.gesture.0) << 3 | COLLECTING)
    }

    pub(crate) fn mode(&self, session: &AtomicBool, now: Instant) -> Result<Mode, MicError> {
        let state = self.gate.load(Ordering::Acquire);
        if state >> 3 != u64::from(self.gesture.0) {
            return Err(MicError::Stale);
        }
        let error = if !session.load(Ordering::Acquire) {
            Some(MicError::SessionLost)
        } else if (state & 7 < COLLECTING
            && self.startup.get().is_some_and(|start| {
                now.saturating_duration_since(*start)
                    >= Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS))
            }))
            || (state & 7 < RELEASED && now.saturating_duration_since(self.at) >= HOLD_LIMIT)
        {
            Some(MicError::Deadline)
        } else {
            None
        };
        if let Some(error) = error {
            self.cancel();
            return Err(error);
        }
        match state & 7 {
            ARMED..=RUNNING => Ok(Mode::Preparing),
            COLLECTING => Ok(Mode::Collecting),
            RELEASED => Ok(Mode::Release),
            _ => Err(MicError::Cancelled),
        }
    }

    pub(super) fn running(&self) -> Result<(), MicError> {
        self.advance(OPENING, RUNNING)
    }
}
