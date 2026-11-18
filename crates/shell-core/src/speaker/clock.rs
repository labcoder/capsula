//! The played-content clock, shared by every platform: which written device frames the stream's
//! own clock says played. An adapter leases frames, commits them after the native release and
//! reports readings; every rule about what counts is here (docs/development/speaker.md).

use std::time::Duration;

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;

/// Intervals of written frames not yet played; running out freezes the clock.
const LEDGER: usize = 64;
/// How long written frames may wait, on the adapter's own clock, for a trusted advance [guess].
pub const STALL: Duration = Duration::from_millis(200);

/// One reading of a stream's clock, as the platform gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The device position, in `frequency` units.
    pub position: u64,
    /// Position units per second.
    pub frequency: u64,
    /// When the device was at `position`, in `host_frequency` units.
    pub host_time: u64,
    /// Host time units per second.
    pub host_frequency: u64,
    /// False when the platform says the reading is late or inexact; such a reading never counts.
    pub precise: bool,
    /// When the adapter took the reading, on its own monotonic clock. The stall deadline runs on
    /// this, never on the device's times.
    pub service: Duration,
}

/// Why the stream's frames can't be trusted any more. A fault freezes the clock for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// The device, or a call on it, failed.
    #[error("the output device failed")]
    Lost,
    /// The position or its time went backward.
    #[error("the output clock went backward")]
    Regressed,
    /// The position passed the last frame written.
    #[error("the output clock passed the last frame written")]
    Jumped,
    /// Written frames waited [`STALL`] without a trusted advance.
    #[error("the output clock stalled")]
    Stalled,
    /// The device may have run out of frames before a write landed, so it may have played
    /// silence nothing mapped.
    #[error("the output device may have run dry")]
    Gap,
    /// More intervals in flight than the ledger holds.
    #[error("too many output intervals in flight")]
    Ledger,
    /// The clock's epoch changed, began twice or never began.
    #[error("the output clock's epoch ended")]
    Epoch,
    /// The brain sent speech that can't be true.
    #[error("the brain sent impossible speech")]
    Broken,
    /// The brain left the speaker's reports unread past their bound.
    #[error("the brain stopped reading the speaker's reports")]
    Backlog,
}

/// What a run of written frames holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    /// Silence nothing asked for; it never counts.
    Filler,
    /// A segment's samples.
    Content {
        /// The segment's index in its run.
        segment: usize,
        /// The segment's count before the first of these frames.
        first: u32,
    },
}

#[derive(Clone, Copy, Debug)]
struct Interval {
    /// The device frame after its last one.
    end: u64,
    tag: Tag,
}

/// Frames staged for the device and not yet committed; dropping it takes nothing.
#[derive(Debug)]
#[must_use]
pub(crate) struct Lease {
    entries: usize,
    frames: u64,
    content_end: Option<u64>,
}

impl Lease {
    /// The device frame after the lease's last content frame, if it holds any.
    pub(crate) fn content(&self) -> Option<u64> {
        self.content_end
    }
}

/// One stream's clock. Its methods never block, log or allocate.
#[derive(Debug)]
pub(crate) struct Clock {
    rate: u32,
    qpc_frequency: u64,
    /// The clock's frequency and the position of frame 0.
    epoch: Option<(u64, u64)>,
    /// The last precise reading's raw position and host time.
    last: Option<(u64, u64)>,
    /// The last precise reading's time, in the wire's QPC ticks.
    now: u64,
    /// The host time units the epoch's first reading pinned.
    host_frequency: Option<u64>,
    /// When written frames last advanced, or the release that left frames outstanding when none
    /// were.
    trusted: Duration,
    /// The first device frame not yet verified played.
    frontier: u64,
    /// The first device frame not yet written.
    written: u64,
    ledger: [Interval; LEDGER],
    head: usize,
    entries: usize,
    /// Where the ledger's first interval starts.
    head_start: u64,
    /// The device frame after the last content frame written.
    content_end: u64,
    /// The device took content the clock couldn't place, so nothing is quiet again.
    unplaced: bool,
    fault: Option<Fault>,
}

impl Clock {
    /// A clock for a device at `rate` frames per second; `qpc_frequency` is the wire's QPC ticks
    /// per second.
    pub(crate) fn new(rate: u32, qpc_frequency: u64) -> Self {
        Self {
            rate,
            qpc_frequency,
            epoch: None,
            last: None,
            now: 0,
            host_frequency: None,
            trusted: Duration::ZERO,
            frontier: 0,
            written: 0,
            ledger: [Interval {
                end: 0,
                tag: Tag::Filler,
            }; LEDGER],
            head: 0,
            entries: 0,
            head_start: 0,
            content_end: 0,
            unplaced: false,
            fault: None,
        }
    }

    /// Starts the stream's one epoch: its clock runs at `frequency`, and frame 0 is at `origin`,
    /// read before the first write.
    ///
    /// # Errors
    /// [`Fault::Epoch`] for a zero frequency or a second epoch, or the fault already frozen.
    pub(crate) fn begin(&mut self, frequency: u64, origin: u64) -> Result<(), Fault> {
        self.check()?;
        if frequency == 0 || self.epoch.is_some() {
            return Err(self.freeze(Fault::Epoch));
        }
        self.epoch = Some((frequency, origin));
        Ok(())
    }

    /// Freezes the clock for good and returns its fault; the first one stays.
    pub(crate) fn freeze(&mut self, fault: Fault) -> Fault {
        *self.fault.get_or_insert(fault)
    }

    /// The fault that froze the clock, if one did.
    pub(crate) fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Starts staging the next frames. `unread` is how many written frames the device holds
    /// before this write: none left after an earlier write means it may already have run dry, so
    /// nothing more is written.
    ///
    /// # Errors
    /// [`Fault::Epoch`] before [`Clock::begin`], [`Fault::Gap`] then, or the fault already
    /// frozen.
    pub(crate) fn lease(&mut self, unread: u32) -> Result<Lease, Fault> {
        self.check()?;
        if self.epoch.is_none() {
            return Err(self.freeze(Fault::Epoch));
        }
        if self.written > 0 && unread == 0 {
            return Err(self.freeze(Fault::Gap));
        }
        Ok(Lease {
            entries: self.entries,
            frames: 0,
            content_end: None,
        })
    }

    /// Adds `frames` frames holding `tag` to the lease.
    ///
    /// # Errors
    /// [`Fault::Ledger`] when the ledger is full, or the fault already frozen.
    pub(crate) fn push(&mut self, lease: &mut Lease, frames: u64, tag: Tag) -> Result<(), Fault> {
        self.check()?;
        if lease.entries == LEDGER {
            return Err(self.freeze(Fault::Ledger));
        }
        lease.frames += frames;
        let end = self.written + lease.frames;
        if tag != Tag::Filler {
            lease.content_end = Some(end);
        }
        self.ledger[(self.head + lease.entries) % LEDGER] = Interval { end, tag };
        lease.entries += 1;
        Ok(())
    }

    /// The device took the lease's frames at `released`, on the adapter's monotonic clock; when no
    /// frames were outstanding before, the deadline runs from then. `unread` is how many written
    /// frames it held right after the release, these included, or `None` when it couldn't say.
    /// When none of the
    /// earlier ones were left, the device may have run dry before these landed, so nothing after
    /// them can be mapped. Content the device took and the clock can't place keeps
    /// [`Clock::quiet`] false for good. A lease of no frames commits nothing.
    ///
    /// # Errors
    /// [`Fault::Gap`], [`Fault::Lost`] when `unread` is `None`, or the fault already frozen.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the lease keeps it from being committed twice"
    )]
    pub(crate) fn commit(
        &mut self,
        lease: Lease,
        unread: Option<u32>,
        released: Duration,
    ) -> Result<(), Fault> {
        if lease.frames == 0 {
            return self.check();
        }
        let placed =
            unread.is_some_and(|unread| self.written == 0 || u64::from(unread) > lease.frames);
        if self.fault.is_some() || !placed {
            self.unplaced |= lease.content_end.is_some();
            self.check()?;
            return Err(self.freeze(if unread.is_some() {
                Fault::Gap
            } else {
                Fault::Lost
            }));
        }
        if self.written == self.frontier {
            self.trusted = released;
        }
        self.written += lease.frames;
        self.entries = lease.entries;
        if let Some(end) = lease.content_end {
            self.content_end = end;
        }
        Ok(())
    }

    /// A reading of the device's clock. Each content interval it reaches is passed to `played`
    /// as its segment and the count it got to.
    ///
    /// # Errors
    /// The fault this reading proves, or the one already frozen.
    pub(crate) fn observe(
        &mut self,
        at: Observation,
        mut played: impl FnMut(usize, u32),
    ) -> Result<(), Fault> {
        self.check()?;
        let Some((frequency, origin)) = self.epoch else {
            return Err(self.freeze(Fault::Epoch));
        };
        if at.frequency != frequency
            || at.host_frequency == 0
            || *self.host_frequency.get_or_insert(at.host_frequency) != at.host_frequency
        {
            return Err(self.freeze(Fault::Epoch));
        }
        if self.written > self.frontier && at.service.saturating_sub(self.trusted) > STALL {
            return Err(self.freeze(Fault::Stalled));
        }
        if !at.precise {
            return Ok(());
        }
        if self
            .last
            .is_some_and(|(position, host)| at.position < position || at.host_time < host)
        {
            return Err(self.freeze(Fault::Regressed));
        }
        let Some(frame) = at
            .position
            .checked_sub(origin)
            .map(|position| scale(position, u64::from(self.rate), frequency))
        else {
            return Err(self.freeze(Fault::Regressed));
        };
        if frame > self.written {
            return Err(self.freeze(Fault::Jumped));
        }
        // A repeated time says nothing about when the position got there, so it's no advance.
        if self.last.is_some_and(|(_, host)| at.host_time == host) {
            return Ok(());
        }
        self.last = Some((at.position, at.host_time));
        self.now = scale(at.host_time, self.qpc_frequency, at.host_frequency);
        if frame > self.frontier {
            self.frontier = frame;
            self.trusted = at.service;
            self.retire(&mut played);
        }
        Ok(())
    }

    /// Every content frame the device took has played.
    pub(crate) fn quiet(&self) -> bool {
        !self.unplaced && self.frontier >= self.content_end
    }

    /// The device frame after the last content frame written.
    pub(crate) fn content_end(&self) -> u64 {
        self.content_end
    }

    /// The first device frame not yet verified played.
    pub(crate) fn frontier(&self) -> u64 {
        self.frontier
    }

    /// The device may have taken content the clock can't place.
    pub(crate) fn unplaced(&self) -> bool {
        self.unplaced
    }

    /// A release that may have exposed content failed, so nothing is quiet again.
    pub(crate) fn unplace(&mut self) {
        self.unplaced = true;
    }

    /// Content frames written and not yet played.
    pub(crate) fn unplayed(&self) -> u64 {
        let mut start = self.head_start;
        let mut unplayed = 0;
        for at in 0..self.entries {
            let entry = self.ledger[(self.head + at) % LEDGER];
            if entry.tag != Tag::Filler {
                unplayed += entry.end.saturating_sub(start.max(self.frontier));
            }
            start = entry.end;
        }
        unplayed
    }

    /// The last precise reading's time, in the wire's QPC ticks.
    pub(crate) fn now(&self) -> u64 {
        self.now
    }

    fn check(&self) -> Result<(), Fault> {
        self.fault.map_or(Ok(()), Err)
    }

    /// Retires the ledger up to the frontier, passing on the content that played.
    fn retire(&mut self, played: &mut impl FnMut(usize, u32)) {
        while self.entries > 0 {
            let entry = self.ledger[self.head];
            let upto = entry.end.min(self.frontier);
            if let Tag::Content { segment, first } = entry.tag
                && upto > self.head_start
            {
                let count = u32::try_from(upto - self.head_start).unwrap_or(u32::MAX);
                played(segment, first.saturating_add(count));
            }
            if entry.end > self.frontier {
                break;
            }
            self.head_start = entry.end;
            self.head = (self.head + 1) % LEDGER;
            self.entries -= 1;
        }
    }
}

/// `value * to / from`, without overflow; saturates past `u64::MAX`.
pub(crate) fn scale(value: u64, to: u64, from: u64) -> u64 {
    u64::try_from(u128::from(value) * u128::from(to) / u128::from(from.max(1))).unwrap_or(u64::MAX)
}
