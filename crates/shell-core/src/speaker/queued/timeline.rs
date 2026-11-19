use std::collections::VecDeque;
use std::time::Duration;

use kumochi_protocol::message::{RunId, SegmentId};

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
    pub(super) tail: Tail,
    physical: Option<u64>,
    pending_open: Option<u64>,
    generation: u64,
    inhibit: Option<(u64, bool)>,
    tail_generation: Option<u64>,
    anchors: VecDeque<Anchor>,
    last_anchor_at: Option<Duration>,
    retry_at: Duration,
    virtual_at: Duration,
    virtual_fraction: u128,
    active_at: Duration,
}

impl Engine {
    pub(super) fn new(run: RunId) -> Self {
        Self {
            queue: Queue::new(run),
            progress: Progress::default(),
            phase: Phase::Idle,
            offered: 0,
            tail: Tail::At(Duration::ZERO),
            physical: None,
            pending_open: None,
            generation: 0,
            inhibit: None,
            tail_generation: None,
            anchors: VecDeque::with_capacity(ANCHORS),
            last_anchor_at: None,
            retry_at: Duration::ZERO,
            virtual_at: Duration::ZERO,
            virtual_fraction: 0,
            active_at: Duration::ZERO,
        }
    }

    pub(super) fn expect(&mut self, now: Duration) {
        if !self.queue.stopped && self.phase == Phase::Idle {
            self.phase = Phase::Buffering(PRIME);
            self.active_at = now;
            self.rebase(now);
        }
    }

    pub(super) fn request(&mut self, now: Duration) -> Option<u64> {
        if self.phase == Phase::Idle
            || self.queue.stopped
            || self.physical.is_some()
            || self.pending_open.is_some()
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
        if self.inhibit.is_none_or(|(previous, _)| serial > previous) {
            self.inhibit = Some((serial, true));
            self.rebase(now);
        }
    }

    pub(super) fn resume(&mut self, serial: u64, now: Duration) {
        if self.inhibit == Some((serial, true)) {
            self.inhibit = Some((serial, false));
            self.rebase(now);
        }
    }

    fn inhibited(&self) -> bool {
        self.inhibit.is_some_and(|(_, held)| held)
    }

    pub(super) fn offer(&mut self, packet: Packet, now: Duration) -> Result<Admission, Fault> {
        let valid = packet.pcm.run == self.queue.run && !self.queue.stopped;
        let admission = self.queue.offer(packet)?;
        if valid && matches!(admission, Admission::Return(_)) {
            self.expect(now);
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

    pub(super) fn observe(&mut self, anchor: Anchor) -> bool {
        let previous = self
            .anchors
            .back()
            .map_or(self.progress.consumed, |last| last.through);
        if self.queue.stopped
            || anchor.run != self.queue.run
            || self.physical != Some(anchor.generation)
            || anchor.first != previous
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
            return false;
        }
        self.last_anchor_at = Some(anchor.at);
        self.anchors.push_back(anchor);
        true
    }

    pub(super) fn lost(&mut self, generation: u64, handed: u64, now: Duration) -> bool {
        if self.physical != Some(generation) {
            return false;
        }
        self.present_due(now);
        let handed = self
            .anchors
            .back()
            .map_or(handed, |anchor| handed.max(anchor.through));
        self.anchors.clear();
        self.physical = None;
        self.tail = Tail::Unknown;
        self.tail_generation = Some(generation);
        if !self.queue.stopped {
            self.consume(handed.min(self.offered), Evidence::Unheard, now);
            self.offered = self.progress.consumed;
            self.phase = Phase::Buffering(REFILL);
            self.rebase(now);
            self.ready(now);
        }
        true
    }

    pub(super) fn acknowledge(&mut self, generation: u64, at: Option<Duration>) -> bool {
        if self.tail_generation != Some(generation) {
            return false;
        }
        self.tail_generation = None;
        self.tail = at.map_or(Tail::Unknown, Tail::At);
        true
    }

    pub(super) fn stop(&mut self, now: Duration) -> (usize, Option<Credit>) {
        if !self.queue.stopped {
            if let Some(generation) = self.physical {
                self.tail_generation = Some(generation);
                self.tail = Tail::Unknown;
            }
            self.phase = Phase::Idle;
            self.anchors.clear();
            self.rebase(now);
        }
        self.queue.stop()
    }

    pub(super) fn final_at(&self, now: Duration) -> bool {
        self.queue.stopped && matches!(self.tail, Tail::At(at) if at <= now)
    }

    pub(super) fn tick(&mut self, now: Duration) -> Result<Option<Credit>, Fault> {
        if self.queue.stopped {
            return Ok(None);
        }
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
        let credit = self.queue.retry()?;
        if self.phase == Phase::Running && self.queue.samples.is_empty() {
            self.phase = Phase::Buffering(REFILL);
            self.rebase(now);
        }
        self.ready(now);
        if self.queue.samples.is_empty() && now.saturating_sub(self.active_at) >= IDLE {
            self.phase = Phase::Idle;
        }
        Ok(credit)
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
