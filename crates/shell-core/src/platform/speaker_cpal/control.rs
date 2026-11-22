use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::Thread;
use std::time::Duration;

use cpal::StreamInstant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TailAck {
    Pending,
    Unknown,
    At(Duration),
}

const STOPPED: u8 = 1;
const INHIBITED: u8 = 2;
const COPYING: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyDenied {
    Stop,
    Silence,
}

impl CopyDenied {
    fn from_gate(gate: u8) -> Self {
        if gate & STOPPED != 0 {
            Self::Stop
        } else {
            Self::Silence
        }
    }
}

#[derive(Default)]
pub(crate) struct Control {
    gate: AtomicU8,
    wake: Option<Thread>,
    tail: AtomicU64,
    handoff: AtomicU64,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    presented_prefix: AtomicU64,
    callbacks: AtomicU64,
    #[cfg(any(test, feature = "test-support"))]
    stall: AtomicU64,
    overflow: AtomicBool,
    clock: OnceLock<(StreamInstant, Duration)>,
}

impl Control {
    pub(crate) fn new(wake: Thread) -> Self {
        Self {
            wake: Some(wake),
            ..Self::default()
        }
    }

    pub(super) fn wake(&self) {
        if let Some(wake) = &self.wake {
            wake.unpark();
        }
    }

    pub(crate) fn stop(&self) {
        self.gate.fetch_or(STOPPED, Ordering::AcqRel);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.gate.load(Ordering::Acquire) & STOPPED != 0
    }

    pub(crate) fn inhibit(&self) {
        self.gate.fetch_or(INHIBITED, Ordering::AcqRel);
    }

    pub(crate) fn resume(&self) {
        self.gate.fetch_and(!INHIBITED, Ordering::AcqRel);
    }

    pub(super) fn interruption(&self) -> Option<CopyDenied> {
        let gate = self.gate.load(Ordering::Acquire);
        (gate != 0).then(|| CopyDenied::from_gate(gate))
    }

    pub(super) fn begin_copy(&self) -> Result<(), CopyDenied> {
        self.gate
            .compare_exchange(0, COPYING, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(CopyDenied::from_gate)
    }

    pub(super) fn end_copy(&self) {
        self.gate.fetch_and(!COPYING, Ordering::Release);
    }

    pub(crate) fn fenced_handoff(&self) -> Option<u64> {
        (self.gate.load(Ordering::Acquire) & (STOPPED | COPYING) == STOPPED).then(|| self.handed())
    }

    pub(super) fn note_callback(&self) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn callback_count(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    /// How long the worker waits for a callback before it recovers the route.
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        expect(
            clippy::unused_self,
            reason = "only fixture devices override the stall"
        )
    )]
    pub(crate) fn stall(&self) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if let millis @ 1.. = self.stall.load(Ordering::Relaxed) {
            return Duration::from_millis(millis);
        }
        crate::speaker::STALL
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_stall(&self, millis: u64) {
        self.stall.store(millis, Ordering::Relaxed);
    }

    pub(super) fn handed(&self) -> u64 {
        self.handoff.load(Ordering::Acquire)
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn record_presented_prefix(&self, through: u64) {
        self.presented_prefix.store(through, Ordering::Release);
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn presented_prefix(&self) -> u64 {
        self.presented_prefix.load(Ordering::Acquire)
    }

    pub(super) fn handed_through(&self, through: u64) {
        self.handoff.fetch_max(through, Ordering::Release);
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
    }

    pub(super) fn overflow(&self) {
        self.overflow.store(true, Ordering::Release);
    }

    pub(crate) fn tail(&self) -> TailAck {
        match self.tail.load(Ordering::Acquire) {
            0 => TailAck::Pending,
            1 => TailAck::Unknown,
            at => TailAck::At(Duration::from_nanos(at - 2)),
        }
    }

    pub(super) fn latch(&self, at: Option<Duration>) {
        let encoded = at
            .and_then(|time| u64::try_from(time.as_nanos()).ok())
            .and_then(|nanos| nanos.checked_add(2))
            .unwrap_or(1);
        let _ = self
            .tail
            .compare_exchange(0, encoded, Ordering::Release, Ordering::Relaxed);
    }

    pub(super) fn calibrate(&self, backend: StreamInstant, local: Duration) {
        let _ = self.clock.set((backend, local));
    }

    pub(super) fn time(&self, when: StreamInstant) -> Option<Duration> {
        let &(backend, local) = self.clock.get()?;
        if let Some(after) = when.checked_duration_since(backend) {
            local.checked_add(after)
        } else {
            local.checked_sub(backend.checked_duration_since(when)?)
        }
    }
}

pub(super) fn frame_time(base: Option<Duration>, frames: usize, rate: u32) -> Option<Duration> {
    let nanos = u64::try_from(frames).ok()?.checked_mul(1_000_000_000)? / u64::from(rate);
    base?.checked_add(Duration::from_nanos(nanos))
}
