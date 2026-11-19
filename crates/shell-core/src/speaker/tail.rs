//! What one output generation publishes for the link's output owner
//! (docs/development/speaker.md): one coherent snapshot, written only by its engine.

use std::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(test)]
#[path = "tail_tests.rs"]
mod tests;

/// Content the device took may be unplaced: a gap, an unknown padding or a failed release.
pub const UNKNOWN: u8 = 1;
/// Nothing more is staged as content: withdrawn or failed.
pub(crate) const SEALED: u8 = 2;
/// The clock froze.
pub const FAULT: u8 = 4;
/// The stream was stopped and let go, so nothing more is released.
pub const RELEASED: u8 = 8;

/// A generation's tail in absolute device frames of its one clock epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The end of the content committed to the device.
    pub exposed: u64,
    /// The verified frontier.
    pub played: u64,
    /// The content end of a release in flight; 0 for none.
    pub flight: u64,
    /// The latest pause serial that went quiet; 0 for none.
    pub quiet: u32,
    /// [`UNKNOWN`], [`SEALED`], [`FAULT`] and [`RELEASED`].
    pub flags: u8,
    /// The run whose content the last successful content release committed, until a stop of it
    /// or a newer run, the seal or the release; 0 for none.
    pub spoken: u64,
}

impl Snapshot {
    /// Content the device may still hold, or that can't be shown played.
    pub fn owed(&self) -> bool {
        self.flags & UNKNOWN != 0 || self.flight != 0 || self.played < self.exposed
    }

    /// What a waiting claim depends on changed.
    pub(crate) fn differs(&self, from: &Self) -> bool {
        self.quiet != from.quiet || self.flags != from.flags || self.owed() != from.owed()
    }
}

/// A sequence lock over one [`Snapshot`]: one writer, and readers that never wait. Its stamp is
/// 64 bits, as cheap as the payload's words, so it can't come back round under a stalled reader.
#[derive(Debug, Default)]
pub struct Tail {
    seq: AtomicU64,
    exposed: AtomicU64,
    played: AtomicU64,
    flight: AtomicU64,
    state: AtomicU64,
    spoken: AtomicU64,
}

impl Tail {
    /// Publishes `now`; only the generation's engine calls it.
    pub(crate) fn write(&self, now: &Snapshot) {
        let seq = self.seq.load(Ordering::Relaxed).wrapping_add(1);
        self.seq.store(seq, Ordering::Relaxed);
        fence(Ordering::Release);
        self.exposed.store(now.exposed, Ordering::Relaxed);
        self.played.store(now.played, Ordering::Relaxed);
        self.flight.store(now.flight, Ordering::Relaxed);
        self.state.store(
            u64::from(now.quiet) << 8 | u64::from(now.flags),
            Ordering::Relaxed,
        );
        self.spoken.store(now.spoken, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Release);
    }

    /// Publishes `now` for a test standing in for the engine.
    #[cfg(any(test, feature = "test-support"))]
    pub fn publish(&self, now: &Snapshot) {
        self.write(now);
    }

    /// The last snapshot, or `None` while one is being written.
    pub fn read(&self) -> Option<Snapshot> {
        let seq = self.seq.load(Ordering::Acquire);
        let state = self.state.load(Ordering::Relaxed);
        let snapshot = Snapshot {
            exposed: self.exposed.load(Ordering::Relaxed),
            played: self.played.load(Ordering::Relaxed),
            flight: self.flight.load(Ordering::Relaxed),
            quiet: u32::try_from(state >> 8).unwrap_or(u32::MAX),
            flags: u8::try_from(state & 0xff).unwrap_or(u8::MAX),
            spoken: self.spoken.load(Ordering::Relaxed),
        };
        self.confirm(seq, snapshot)
    }

    /// `snapshot`, read after the stamp `seq`, when no write was in progress or came since.
    fn confirm(&self, seq: u64, snapshot: Snapshot) -> Option<Snapshot> {
        fence(Ordering::Acquire);
        (seq.is_multiple_of(2) && self.seq.load(Ordering::Relaxed) == seq).then_some(snapshot)
    }
}
