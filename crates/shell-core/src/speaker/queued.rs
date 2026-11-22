//! Per-run source queue and playback state for the internal queued output path.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    MAX_AHEAD_SEGMENTS, MAX_SEGMENTS_PER_RUN, MAX_UNPLAYED_SECONDS, OutputGeneration, RunId,
    SegmentId, SpeechPcm,
};
use zeroize::Zeroize as _;

#[cfg(any(windows, target_os = "macos"))]
mod backend;
mod receipts;
mod timeline;
#[cfg(any(windows, target_os = "macos"))]
mod worker;

#[cfg(any(test, feature = "test-support"))]
pub use backend::{FixturePlayback, await_fake_disposal, serialize_playback};

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub(crate) use worker::FakeOutput;
pub(crate) use worker::{Command, DrawingIo, Input, Ordered, OwnerExit, spawn, spawn_plain};
pub(crate) const RATE: u32 = 24_000;
const CAPACITY: usize = 480_000;
const SEGMENT_SLOTS: usize = 256;
const _: () = assert!(
    MAX_SEGMENTS_PER_RUN == 256,
    "source gate slots cover every wire segment"
);
const _: () = assert!(
    RATE * MAX_UNPLAYED_SECONDS == 480_000,
    "capacity follows the wire source window"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Credit {
    origin: u64,
    bytes: u32,
}

pub(crate) struct Packet {
    credit: Credit,
    pcm: SpeechPcm,
}

impl Packet {
    pub(crate) fn wire(pcm: SpeechPcm, origin: u64, bytes: u32) -> Self {
        Self {
            pcm,
            credit: Credit { origin, bytes },
        }
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packet")
            .field("credit", &self.credit)
            .field("pcm", &self.pcm)
            .finish()
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        self.pcm.samples.zeroize();
    }
}

/// How long a presented frame keeps speech evidence.
const SPEECH_GRACE: Duration = Duration::from_millis(500);

/// Expiring evidence that a run's audio is presenting: it holds Esc, never admits a microphone.
#[derive(Clone)]
pub(crate) struct Speech {
    base: Instant,
    until: Arc<AtomicU64>,
    changed: async_channel::Sender<()>,
}

impl Speech {
    pub(crate) fn new(changed: async_channel::Sender<()>) -> Self {
        Self {
            base: Instant::now(),
            until: Arc::default(),
            changed,
        }
    }

    /// Holds to a grace past the frame presented at `presented`, on a clock now at `now`.
    fn hold(&self, presented: Duration, now: Duration) {
        let Some(left) = presented
            .saturating_add(SPEECH_GRACE)
            .checked_sub(now)
            .filter(|left| !left.is_zero())
        else {
            return;
        };
        let until = Instant::now() + left;
        let nanos = u64::try_from(until.saturating_duration_since(self.base).as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        let before = self.at(self.until.fetch_max(nanos, Ordering::AcqRel));
        if before.is_none_or(|at| at <= Instant::now()) {
            let _woken = self.changed.try_send(());
        }
    }

    fn clear(&self) {
        if self.until.swap(0, Ordering::AcqRel) != 0 {
            let _woken = self.changed.try_send(());
        }
    }

    /// When the evidence runs out, while it still holds.
    pub(crate) fn until(&self) -> Option<Instant> {
        self.at(self.until.load(Ordering::Acquire))
            .filter(|at| *at > Instant::now())
    }

    fn at(&self, nanos: u64) -> Option<Instant> {
        (nanos != 0).then(|| self.base + Duration::from_nanos(nanos))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Return(Credit),
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Delivery,
    Order,
    Size,
}

struct Segment {
    id: SegmentId,
    first: u64,
    samples: u32,
    next_seq: u32,
    ended: bool,
}

impl Segment {
    fn end(&self) -> u64 {
        self.first + u64::from(self.samples)
    }
}

struct Queue {
    run: RunId,
    logical: OutputGeneration,
    stopped: bool,
    retired: u64,
    received: u64,
    next_segment: u32,
    completed: u32,
    completed_samples: [u32; SEGMENT_SLOTS],
    discarded: [bool; SEGMENT_SLOTS],
    staged: [bool; SEGMENT_SLOTS],
    unheard: [bool; SEGMENT_SLOTS],
    samples: VecDeque<i16>,
    segments: VecDeque<Segment>,
    held: Option<Packet>,
}

impl Queue {
    fn new(run: RunId, logical: OutputGeneration) -> Self {
        Self {
            run,
            logical,
            stopped: false,
            retired: 0,
            received: 0,
            next_segment: 0,
            completed: 0,
            completed_samples: [0; SEGMENT_SLOTS],
            discarded: [false; SEGMENT_SLOTS],
            staged: [false; SEGMENT_SLOTS],
            unheard: [false; SEGMENT_SLOTS],
            samples: VecDeque::with_capacity(CAPACITY),
            segments: VecDeque::new(),
            held: None,
        }
    }

    fn segment(&self, id: SegmentId) -> Option<&Segment> {
        self.segments.iter().find(|segment| segment.id == id)
    }

    fn discarded(&self, id: SegmentId) -> bool {
        usize::try_from(id.0)
            .ok()
            .and_then(|index| self.discarded.get(index))
            .copied()
            .unwrap_or(false)
    }

    fn note_staged(&mut self, id: SegmentId) {
        if let Ok(index) = usize::try_from(id.0)
            && let Some(staged) = self.staged.get_mut(index)
        {
            *staged = true;
        }
    }

    fn was_staged(&self, id: SegmentId) -> bool {
        usize::try_from(id.0).is_ok_and(|index| self.staged.get(index) == Some(&true))
    }

    fn has_unheard(&self, id: SegmentId) -> bool {
        usize::try_from(id.0).is_ok_and(|index| self.unheard.get(index) == Some(&true))
    }

    fn note_unheard(&mut self, through: u64) {
        for segment in self
            .segments
            .iter()
            .take_while(|segment| segment.first < through)
        {
            if segment.samples != 0
                && segment.end() > self.retired
                && let Ok(index) = usize::try_from(segment.id.0)
                && let Some(unheard) = self.unheard.get_mut(index)
            {
                *unheard = true;
            }
        }
    }

    fn current(&self, pcm: &SpeechPcm) -> bool {
        !self.stopped && pcm.run == self.run && pcm.output_generation == self.logical
    }

    fn offer(&mut self, packet: Packet) -> Result<Admission, Fault> {
        if packet.pcm.output_generation != self.logical {
            return Ok(Admission::Return(packet.credit));
        }
        if packet.pcm.run > self.run {
            return Err(Fault::Order);
        }
        if self.stopped || packet.pcm.run < self.run {
            return Ok(Admission::Return(packet.credit));
        }
        if self.held.is_some() {
            return Err(Fault::Order);
        }
        let pcm = &packet.pcm;
        pcm.check(RATE).map_err(|_| Fault::Size)?;
        let continuing = self.segments.back().filter(|segment| !segment.ended);
        let (samples, next_seq) = if let Some(segment) = continuing {
            if segment.id != pcm.segment {
                return Err(Fault::Order);
            }
            (segment.samples, segment.next_seq)
        } else {
            if pcm.segment.0 != self.next_segment || self.next_segment >= MAX_SEGMENTS_PER_RUN {
                return Err(Fault::Order);
            }
            (0, 0)
        };
        if pcm.seq != next_seq {
            return Err(Fault::Order);
        }
        let count = u32::try_from(pcm.samples.len()).map_err(|_| Fault::Size)?;
        let total = samples.checked_add(count).ok_or(Fault::Size)?;
        if total > RATE * kumochi_protocol::message::MAX_SEGMENT_SECONDS
            || pcm.last.is_some_and(|last| last != total)
        {
            return Err(Fault::Size);
        }
        let ahead = u32::try_from(self.segments.len()).map_err(|_| Fault::Size)?;
        if self.samples.len() + pcm.samples.len() > CAPACITY
            || continuing.is_none() && ahead > MAX_AHEAD_SEGMENTS
        {
            self.held = Some(packet);
            return Ok(Admission::Held);
        }
        if continuing.is_none() {
            self.segments.push_back(Segment {
                id: pcm.segment,
                first: self.received,
                samples: 0,
                next_seq: 0,
                ended: false,
            });
            self.next_segment += 1;
        }
        let segment = self.segments.back_mut().ok_or(Fault::Order)?;
        segment.samples = total;
        segment.next_seq = next_seq.checked_add(1).ok_or(Fault::Size)?;
        segment.ended = pcm.last.is_some();
        self.received += u64::from(count);
        self.samples.extend(pcm.samples.iter().copied());
        self.complete_segments();
        Ok(Admission::Return(packet.credit))
    }

    fn retry(&mut self) -> Result<Option<Credit>, Fault> {
        let Some(packet) = self.held.take() else {
            return Ok(None);
        };
        match self.offer(packet)? {
            Admission::Return(credit) => Ok(Some(credit)),
            Admission::Held => Ok(None),
        }
    }

    fn retire(&mut self, count: usize) -> usize {
        let count = count.min(self.samples.len());
        for sample in self.samples.range_mut(..count) {
            sample.zeroize();
        }
        self.samples.drain(..count);
        self.retired += u64::try_from(count).expect("bounded source queue");
        self.complete_segments();
        count
    }

    fn complete_segments(&mut self) {
        while self
            .segments
            .front()
            .is_some_and(|segment| segment.ended && segment.end() <= self.retired)
        {
            let segment = self
                .segments
                .pop_front()
                .expect("checked the completed segment");
            let index = usize::try_from(self.completed).expect("bounded segment id");
            self.completed_samples[index] = segment.samples;
            self.completed += 1;
        }
    }

    fn stop(&mut self) -> (usize, Option<Credit>) {
        self.stopped = true;
        let discarded = self.samples.len();
        for sample in &mut self.samples {
            sample.zeroize();
        }
        self.samples.clear();
        self.segments.clear();
        (discarded, self.held.take().map(|packet| packet.credit))
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        for sample in &mut self.samples {
            sample.zeroize();
        }
    }
}
