use std::collections::VecDeque;
use std::time::Duration;

use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId};

use super::{Admission, Credit, Fault, Packet, Queue, RATE};

const PRIME: u64 = 1_440;
const REFILL: u64 = 960;
const IDLE: Duration = Duration::from_millis(1_500);
const RETRY: Duration = Duration::from_secs(1);
const ANCHORS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Idle,
    Buffering(u64),
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tail {
    Unknown,
    At(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(super) enum Observation {
    Accepted,
    Stale,
    Recover,
}

#[derive(Clone, Copy)]
pub(super) struct Anchor {
    pub(super) run: RunId,
    pub(super) generation: u64,
    pub(super) segment: SegmentId,
    pub(super) first: u64,
    pub(super) through: u64,
    pub(super) at: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Opened {
    Physical,
    Unavailable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Evidence {
    Device,
    Unheard,
}

#[derive(Default)]
pub(super) struct Progress {
    pub(super) consumed: u64,
    pub(super) heard: u64,
    pub(super) first_gap: Option<u64>,
}

pub(super) struct Engine {
    pub(super) queue: Queue,
    pub(super) progress: Progress,
    pub(super) phase: Phase,
    pub(super) offered: u64,
    physical: Option<u64>,
    pending_open: Option<u64>,
    generation: u64,
    inhibit: Option<(u64, bool)>,
    unresolved: [Option<u64>; 2],
    quiet_at: Duration,
    anchors: VecDeque<Anchor>,
    last_anchor_at: Option<Duration>,
    retry_at: Duration,
    virtual_at: Duration,
    virtual_fraction: u128,
    active_at: Duration,
}

impl Engine {
    pub(super) fn new(run: RunId, logical: OutputGeneration) -> Self {
        Self {
            queue: Queue::new(run, logical),
            progress: Progress::default(),
            phase: Phase::Idle,
            offered: 0,
            physical: None,
            pending_open: None,
            generation: 0,
            inhibit: None,
            unresolved: [None; 2],
            quiet_at: Duration::ZERO,
            anchors: VecDeque::with_capacity(ANCHORS),
            last_anchor_at: None,
            retry_at: Duration::ZERO,
            virtual_at: Duration::ZERO,
            virtual_fraction: 0,
            active_at: Duration::ZERO,
        }
    }

    pub(super) fn expect(&mut self, now: Duration) {
        self.settle(now);
        self.warm(now);
    }

    fn warm(&mut self, now: Duration) {
        if !self.queue.stopped && self.phase == Phase::Idle {
            self.phase = Phase::Buffering(PRIME);
            self.active_at = now;
            self.rebase(now);
        }
    }

    pub(super) fn request(&mut self, now: Duration) -> Option<u64> {
        self.settle(now);
        if self.phase == Phase::Idle
            || self.queue.stopped
            || self.physical.is_some()
            || self.pending_open.is_some()
            || self.unresolved.iter().all(Option::is_some)
            || self.inhibited()
            || now < self.retry_at
        {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.pending_open = Some(self.generation);
        self.retry_at = now.saturating_add(RETRY);
        Some(self.generation)
    }

    pub(super) fn opened(&mut self, generation: u64, outcome: Opened, now: Duration) -> bool {
        self.settle(now);
        if self.pending_open != Some(generation) {
            return false;
        }
        self.pending_open = None;
        if self.queue.stopped {
            return false;
        }
        if outcome == Opened::Physical {
            self.last_anchor_at = None;
            self.physical = Some(generation);
            self.offered = self.progress.consumed;
            self.rebase(now);
        }
        true
    }

    pub(super) fn pause(&mut self, serial: u64, now: Duration) {
        self.settle(now);
        if self.inhibit.is_none_or(|(previous, _)| serial > previous) {
            self.inhibit = Some((serial, true));
            self.virtual_at = self.virtual_at.max(now);
        }
    }

    pub(super) fn resume(&mut self, serial: u64, now: Duration) {
        self.settle(now);
        if self.inhibit == Some((serial, true)) {
            self.inhibit = Some((serial, false));
            self.virtual_at = self.virtual_at.max(now);
        }
    }

    fn inhibited(&self) -> bool {
        self.inhibit.is_some_and(|(_, held)| held)
    }

    pub(super) fn offer(&mut self, packet: Packet, now: Duration) -> Result<Admission, Fault> {
        let valid = self.queue.current(&packet.pcm);
        if valid {
            self.settle(now);
        }
        let admission = self.queue.offer(packet)?;
        if valid && matches!(admission, Admission::Return(_)) {
            self.warm(now);
            self.active_at = now;
            self.ready(now);
        }
        Ok(admission)
    }

    pub(super) fn staged(&mut self, through: u64) {
        if self.physical.is_some() && !self.inhibited() && self.phase == Phase::Running {
            self.offered = self.offered.max(through.min(self.queue.received));
        }
    }

    /// Recover means retire the route with its handed-off boundary before the next tick.
    pub(super) fn observe(&mut self, anchor: Anchor) -> Observation {
        let previous = self
            .anchors
            .back()
            .map_or(self.progress.consumed, |last| last.through);
        if self.queue.stopped
            || anchor.run != self.queue.run
            || self.physical != Some(anchor.generation)
            || anchor.through <= previous
        {
            return Observation::Stale;
        }
        if anchor.first != previous
            || anchor.through <= anchor.first
            || anchor.through > self.offered
            || self.anchors.len() == ANCHORS
            || self.last_anchor_at.is_some_and(|at| at > anchor.at)
            || !self.queue.segments.iter().any(|segment| {
                segment.id == anchor.segment
                    && segment.first <= anchor.first
                    && anchor.through <= segment.end()
            })
        {
            return Observation::Recover;
        }
        self.last_anchor_at = Some(anchor.at);
        self.anchors.push_back(anchor);
        Observation::Accepted
    }

    /// Delivery time cannot confirm a pending prediction on a route already lost.
    pub(super) fn lost(&mut self, generation: u64, handed: u64, now: Duration) -> bool {
        if self.physical != Some(generation) {
            return false;
        }
        let handed = self
            .anchors
            .back()
            .map_or(handed, |anchor| handed.max(anchor.through));
        self.anchors.clear();
        self.physical = None;
        self.wait_for_tail(generation);
        if !self.queue.stopped {
            self.consume(handed.min(self.offered), Evidence::Unheard, now);
            self.offered = self.progress.consumed;
            self.phase = Phase::Buffering(REFILL);
            self.rebase(now);
            self.ready(now);
        }
        true
    }

    fn wait_for_tail(&mut self, generation: u64) {
        let slot = self
            .unresolved
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("a physical open reserves a tail slot");
        *slot = Some(generation);
    }

    pub(super) fn acknowledge(&mut self, generation: u64, at: Option<Duration>) -> bool {
        let Some(slot) = self
            .unresolved
            .iter_mut()
            .find(|slot| **slot == Some(generation))
        else {
            return false;
        };
        if let Some(at) = at {
            *slot = None;
            self.quiet_at = self.quiet_at.max(at);
        }
        true
    }

    pub(super) fn stop(&mut self, now: Duration) -> (usize, Option<Credit>) {
        if self.queue.stopped {
            return (0, None);
        }
        self.queue.stopped = true;
        self.settle(now);
        if let Some(generation) = self.physical.take() {
            self.wait_for_tail(generation);
        }
        self.phase = Phase::Idle;
        self.anchors.clear();
        self.rebase(now);
        self.queue.stop()
    }

    pub(super) fn tail(&self) -> Tail {
        if self.unresolved.iter().any(Option::is_some) {
            Tail::Unknown
        } else {
            Tail::At(self.quiet_at)
        }
    }

    pub(super) fn final_at(&self, now: Duration) -> bool {
        self.queue.stopped && matches!(self.tail(), Tail::At(at) if at <= now)
    }

    pub(super) fn tick(&mut self, now: Duration) -> Result<Option<Credit>, Fault> {
        if self.queue.stopped {
            return Ok(None);
        }
        self.settle(now);
        let credit = self.queue.retry()?;
        if credit.is_some() && !self.queue.samples.is_empty() {
            self.warm(now);
            self.active_at = now;
        }
        self.ready(now);
        Ok(credit)
    }

    fn settle(&mut self, now: Duration) {
        if self.physical.is_some() {
            self.present_due(now);
        } else if !self.inhibited() && self.phase == Phase::Running {
            let elapsed = now.saturating_sub(self.virtual_at).as_nanos();
            self.virtual_at = self.virtual_at.max(now);
            let numerator = elapsed.saturating_mul(u128::from(RATE)) + self.virtual_fraction;
            self.virtual_fraction = numerator % 1_000_000_000;
            let add = u64::try_from(numerator / 1_000_000_000).unwrap_or(u64::MAX);
            let through = self
                .progress
                .consumed
                .saturating_add(add)
                .min(self.queue.received);
            self.consume(through, Evidence::Unheard, now);
        }
        if self.phase == Phase::Running && self.queue.samples.is_empty() {
            self.phase = Phase::Buffering(REFILL);
            self.rebase(now);
        }
        if self.queue.samples.is_empty() && now.saturating_sub(self.active_at) >= IDLE {
            self.phase = Phase::Idle;
        }
    }

    fn ready(&mut self, now: Duration) {
        if let Phase::Buffering(target) = self.phase
            && (!self.queue.samples.is_empty()
                && (self.queue.received - self.queue.retired >= target
                    || self
                        .queue
                        .segments
                        .front()
                        .is_some_and(|segment| segment.ended)))
        {
            self.phase = Phase::Running;
            self.rebase(now);
        }
    }

    fn rebase(&mut self, now: Duration) {
        self.virtual_at = now;
        self.virtual_fraction = 0;
    }

    fn present_due(&mut self, now: Duration) {
        while self.anchors.front().is_some_and(|anchor| anchor.at <= now) {
            let Some(anchor) = self.anchors.pop_front() else {
                break;
            };
            self.consume(anchor.through, Evidence::Device, now);
        }
    }

    fn consume(&mut self, through: u64, evidence: Evidence, now: Duration) {
        let count = usize::try_from(through.saturating_sub(self.progress.consumed))
            .expect("a bounded source span");
        let retired = self.queue.retire(count);
        if retired == 0 {
            return;
        }
        if evidence == Evidence::Unheard && self.progress.first_gap.is_none() {
            self.progress.first_gap = Some(self.progress.consumed);
        }
        self.progress.consumed = self.queue.retired;
        if self.progress.first_gap.is_none() {
            self.progress.heard = self.progress.consumed;
        }
        self.active_at = now;
    }
}
