//! The speaker's owner, shared by every platform: one run's queued speech in a 20 s arena, the
//! reports it owes the brain, and the [`Clock`] that says which of its written frames played.
//! A device adapter only stages, commits and observes (docs/development/speaker.md).

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::message::{
    ControlToBrain, Cursor, MAX_AHEAD_SEGMENTS, MAX_SEGMENT_SECONDS, MAX_SEGMENTS_PER_RUN,
    MAX_UNPLAYED_SECONDS, OutputGeneration, PROGRESS_INTERVAL_MS, RunId, SegmentId, SpeechPcm,
};
use zeroize::Zeroize;

use super::clock::{Clock, Fault, Lease, Observation, Tag, scale};
use super::tail::{FAULT, RELEASED, SEALED, Snapshot, Tail, UNKNOWN};

#[cfg(test)]
#[path = "engine_tests.rs"]
pub(super) mod tests;

/// A speech frame from the brain.
pub enum Data {
    /// [`DataToShell::SegmentQueued`](kumochi_protocol::message::DataToShell::SegmentQueued).
    Queued {
        /// The run.
        run: RunId,
        /// The segment.
        segment: SegmentId,
    },
    /// Audio of a queued segment.
    Pcm(SpeechPcm),
}

impl Data {
    fn run(&self) -> RunId {
        match self {
            Self::Queued { run, .. } => *run,
            Self::Pcm(pcm) => pcm.run,
        }
    }
}

impl fmt::Debug for Data {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued { run, segment } => f
                .debug_struct("Queued")
                .field("run", run)
                .field("segment", segment)
                .finish(),
            Self::Pcm(pcm) => pcm.fmt(f),
        }
    }
}

/// What became of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// It's the current run's, and it's kept.
    Kept,
    /// Fenced, superseded or for another generation: dropped.
    Stale,
    /// A newer run's, while the old one still owns the device: offer it again later.
    Held,
}

#[derive(Debug, Default)]
struct Segment {
    received: u32,
    chunks: u32,
    last: Option<u32>,
    played: u32,
}

/// Where staging got to, applied only at commit.
#[derive(Debug)]
struct Next {
    staging: usize,
    staged: u32,
    read: usize,
    consumed: usize,
}

/// How a device buffer holding staged frames goes back to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// As written: the frames are owed once the release succeeds.
    Content,
    /// As silence: the link let go, and nothing staged is taken.
    Silent,
}

/// Device frames staged and not yet committed. While it lives the engine takes nothing else;
/// dropping it takes nothing back.
#[derive(Debug)]
#[must_use]
pub struct Staged<'a> {
    engine: &'a mut Engine,
    lease: Lease,
    next: Next,
}

impl Staged<'_> {
    /// Gives back a device buffer the staged frames were copied into. Its kind is decided here,
    /// by the last look at revocation, right before `put` makes the native release and returns
    /// when it succeeded: a revoke after it can't recall the buffer, so released content is owed
    /// like any other. `unread` reads the padding after a content release, as [`Staged::commit`]
    /// takes it. A failed release of a buffer holding content may have exposed it, so it's
    /// unplaced for good.
    ///
    /// # Errors
    /// `put`'s fault, or the fault that froze the clock, now or before.
    pub fn release(
        self,
        put: impl FnOnce(Release) -> Result<Duration, Fault>,
        unread: impl FnOnce() -> Option<u32>,
    ) -> Result<(), Fault> {
        if self.engine.revoked.load(Ordering::Acquire) {
            let put = put(Release::Silent);
            if put.is_err() {
                self.engine.lose();
            } else {
                self.engine.land();
            }
            return put.map(drop);
        }
        match put(Release::Content) {
            Ok(released) => self.commit(unread(), released),
            Err(fault) => {
                self.engine.lose();
                Err(fault)
            }
        }
    }

    /// The platform lent no buffer, so nothing staged reached the device.
    pub fn abandon(self) {
        self.engine.land();
    }

    /// The native release succeeded at `released` (see [`Clock::commit`]). `unread` is how many
    /// written frames the device held right after it, these included, or `None` when the device
    /// couldn't say. Content released and
    /// not placed on the clock keeps every later stop and pause from going quiet.
    ///
    /// # Errors
    /// The fault that froze the clock, now or before.
    pub fn commit(self, unread: Option<u32>, released: Duration) -> Result<(), Fault> {
        let Self {
            engine,
            lease,
            next,
        } = self;
        let committed = engine.clock.commit(lease, unread, released);
        engine.flight = 0;
        if let Err(fault) = committed {
            return Err(engine.fail(fault));
        }
        let cap = engine.arena.len();
        let (mut zeroed, mut from) = (next.consumed, engine.read);
        while zeroed > 0 {
            let n = zeroed.min(cap - from);
            engine.arena[from..from + n].fill(0);
            zeroed -= n;
            from = (from + n) % cap;
        }
        engine.staging = next.staging;
        engine.staged = next.staged;
        engine.read = next.read;
        engine.queued -= next.consumed;
        engine.publish();
        Ok(())
    }
}

/// One connection's speaker state. Its methods never block, log or allocate past the bounds set
/// up in [`Engine::new`].
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is an independent latch of the one worker's state"
)]
pub struct Engine {
    clock: Clock,
    rate: u32,
    generation: OutputGeneration,
    qpc_frequency: u64,
    arena: Box<[i16]>,
    read: usize,
    queued: usize,
    run: Option<RunId>,
    named: Option<RunId>,
    fence: Option<RunId>,
    segments: Vec<Segment>,
    staging: usize,
    staged: u32,
    reported: usize,
    progressed: u32,
    progress_at: Option<u64>,
    cursor: Option<Cursor>,
    stopping: bool,
    pause: Option<u32>,
    quiet: Option<u32>,
    retired: Option<(RunId, Option<Cursor>)>,
    /// The runs whose stop was answered last, so a repeat among them isn't answered again.
    answered: [Option<RunId>; ANSWERED],
    answers: usize,
    withdrawn: bool,
    /// A report found the queue full: the brain isn't reading, so the speaker ends.
    backlogged: bool,
    /// Set once the link lets go, from its thread; nothing more is staged as content.
    revoked: Arc<AtomicBool>,
    tail: Arc<Tail>,
    /// The content end of the release in flight; 0 for none.
    flight: u64,
    /// The stream was stopped and let go.
    released: bool,
    published: Snapshot,
    /// What a waiting claim depends on changed since the worker last woke the owner.
    changed: bool,
    pub(super) reports: VecDeque<ControlToBrain>,
}

/// How many answered stops are remembered [guess].
const ANSWERED: usize = 8;

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("rate", &self.rate)
            .field("generation", &self.generation)
            .field("queued", &self.queued)
            .field("run", &self.run)
            .field("fault", &self.clock.fault())
            .field("reports", &self.reports.len())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// A speaker at the device's checked `rate`, for output `generation`. Its first report
    /// announces the output. `qpc_frequency` is the wire's QPC ticks per second; `revoked` is set
    /// once the link lets go of the speaker; `tail` is where it publishes what it owes.
    pub fn new(
        rate: u32,
        generation: OutputGeneration,
        qpc_frequency: u64,
        revoked: Arc<AtomicBool>,
        tail: Arc<Tail>,
    ) -> Self {
        let arena = usize::try_from(u64::from(rate) * u64::from(MAX_UNPLAYED_SECONDS))
            .expect("20 s at the highest rate fits in usize");
        let mut reports = VecDeque::with_capacity(max_reports() + 1);
        reports.push_back(ControlToBrain::AudioOutput { rate, generation });
        Self {
            clock: Clock::new(rate, qpc_frequency),
            rate,
            generation,
            qpc_frequency,
            arena: vec![0; arena].into_boxed_slice(),
            read: 0,
            queued: 0,
            run: None,
            named: None,
            fence: None,
            segments: Vec::with_capacity(max_segments()),
            staging: 0,
            staged: 0,
            reported: 0,
            progressed: 0,
            progress_at: None,
            cursor: None,
            stopping: false,
            pause: None,
            quiet: None,
            retired: None,
            answered: [None; ANSWERED],
            answers: 0,
            withdrawn: false,
            backlogged: false,
            revoked,
            tail,
            flight: 0,
            released: false,
            published: Snapshot::default(),
            changed: false,
            reports,
        }
    }

    /// Starts the stream's one epoch (see [`Clock::begin`]).
    ///
    /// # Errors
    /// The fault that froze the clock, now or before.
    pub fn begin(&mut self, frequency: u64, origin: u64) -> Result<(), Fault> {
        self.clock
            .begin(frequency, origin)
            .map_err(|fault| self.fail(fault))
    }

    /// Writes mono samples for the next `out.len()` device frames, consuming nothing until the
    /// returned [`Staged`] commits. `unread` is how many written frames the device still holds.
    /// Once the speaker is revoked, it stages only silence.
    ///
    /// # Errors
    /// The fault that froze the clock, now or before.
    pub fn stage(&mut self, out: &mut [i16], unread: u32) -> Result<Staged<'_>, Fault> {
        let mut lease = match self.clock.lease(unread) {
            Ok(lease) => lease,
            Err(fault) => return Err(self.fail(fault)),
        };
        let mut next = Next {
            staging: self.staging,
            staged: self.staged,
            read: self.read,
            consumed: 0,
        };
        let live = !self.revoked.load(Ordering::Acquire) && !self.withdrawn;
        let mut at = 0;
        while at < out.len() {
            let available = if live && self.pause.is_none() && !self.stopping {
                self.available(&mut next)
            } else {
                None
            };
            let (n, tag) = if let Some(available) = available {
                let n = available.min(out.len() - at);
                self.copy(next.read, &mut out[at..at + n]);
                let tag = Tag::Content {
                    segment: next.staging,
                    first: next.staged,
                };
                next.staged += count(n);
                next.read = (next.read + n) % self.arena.len();
                next.consumed += n;
                (n, tag)
            } else {
                out[at..].fill(0);
                (out.len() - at, Tag::Filler)
            };
            if let Err(fault) = self.clock.push(&mut lease, wide(n), tag) {
                return Err(self.fail(fault));
            }
            at += n;
        }
        if let Some(end) = lease.content() {
            self.flight = end;
            self.publish();
        }
        Ok(Staged {
            engine: self,
            lease,
            next,
        })
    }

    /// A reading of the device's clock: what played moves on, and the reports and quiet answers
    /// it makes true go out.
    ///
    /// # Errors
    /// The fault that froze the clock, now or before.
    pub fn observe(&mut self, at: Observation) -> Result<(), Fault> {
        let Self {
            clock,
            segments,
            cursor,
            ..
        } = self;
        let observed = clock.observe(at, |segment, count| {
            if let Some(found) = segments.get_mut(segment) {
                found.played = found.played.max(count);
                advance(cursor, segment, count);
            }
        });
        if let Err(fault) = observed {
            return Err(self.fail(fault));
        }
        self.report();
        Ok(())
    }

    /// Freezes the clock for good and withdraws the output (see [`Engine::withdraw`]): a stop is
    /// answered only when nothing written is unplayed.
    pub fn fail(&mut self, fault: Fault) -> Fault {
        let fault = self.clock.freeze(fault);
        self.withdraw();
        fault
    }

    /// Withdraws the output, once: unwritten speech is discarded, nothing more is written as
    /// content and later frames are stale. What was verified played stays, and the clock runs
    /// on, so a stop is still answered once what was written has played.
    pub fn withdraw(&mut self) {
        if !self.withdrawn {
            self.withdrawn = true;
            self.discard();
            // Past the bound: `new` keeps room for this one.
            self.reports
                .push_back(ControlToBrain::AudioOutputUnavailable {
                    generation: self.generation,
                });
        }
        self.publish();
    }

    /// Withdrawn or failed: nothing more is written as content.
    pub fn sealed(&self) -> bool {
        self.withdrawn
    }

    /// Every content frame the device took has played.
    pub fn drained(&self) -> bool {
        self.clock.quiet()
    }

    /// The stream was stopped and let go, so nothing more can be released.
    pub fn released(&mut self) {
        self.released = true;
        self.publish();
    }

    /// Publishes the tail (see [`Tail`]).
    pub(super) fn publish(&mut self) {
        let flags = [
            (self.clock.unplaced(), UNKNOWN),
            (self.withdrawn, SEALED),
            (self.clock.fault().is_some(), FAULT),
            (self.released, RELEASED),
        ];
        let now = Snapshot {
            exposed: self.clock.content_end(),
            played: self.clock.frontier(),
            flight: self.flight,
            quiet: self.quiet.unwrap_or(0),
            flags: flags
                .into_iter()
                .filter_map(|(set, flag)| set.then_some(flag))
                .fold(0, |all, flag| all | flag),
        };
        self.changed |= now.differs(&self.published);
        self.published = now;
        self.tail.write(&now);
    }

    /// Whether a waiting claim's inputs changed since the last call.
    pub(super) fn changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Resolves the release in flight, which exposed nothing.
    fn land(&mut self) {
        self.flight = 0;
        self.publish();
    }

    /// Resolves the release in flight, whose content may have reached the device unplaced.
    fn lose(&mut self) {
        if self.flight != 0 {
            self.clock.unplace();
        }
        self.land();
    }

    /// The fault that froze the clock, if one did.
    pub fn fault(&self) -> Option<Fault> {
        self.clock.fault()
    }

    /// A report found the queue full, whatever faulted first, so the speaker must end.
    pub fn backlogged(&self) -> bool {
        self.backlogged
    }

    /// A frame from the brain.
    ///
    /// # Errors
    /// [`Fault::Broken`] when it can't be true; the speaker is withdrawn.
    pub fn data(&mut self, data: &Data) -> Result<Taken, Fault> {
        let run = data.run();
        let other = matches!(data, Data::Pcm(pcm) if pcm.output_generation != self.generation);
        if self.withdrawn || other || self.fence.is_some_and(|fence| run <= fence) {
            return Ok(Taken::Stale);
        }
        // Only the run control named last may start; the playing run may finish its own.
        if self.run != Some(run) && self.named != Some(run) {
            return Ok(if self.named.is_none_or(|named| run > named) {
                Taken::Held
            } else {
                Taken::Stale
            });
        }
        if self.run.is_some_and(|current| current < run) && !self.supersede() {
            return Ok(Taken::Held);
        }
        self.run = Some(run);
        let kept = match data {
            Data::Queued { segment, .. } => self.queue(*segment),
            Data::Pcm(pcm) => self.pcm(pcm),
        };
        kept.map(|()| Taken::Kept)
            .map_err(|()| self.fail(Fault::Broken))
    }

    /// The brain's revoke or a local Stop: `run` and every older run take no more speech, and
    /// `run`'s `SpeechStopped` goes out once its last written content frame has played. A
    /// repeat within the last 8 answers is not answered again; stops come out of
    /// order, so an older run's first one still is.
    pub fn stop(&mut self, run: RunId) {
        self.fence = self.fence.max(Some(run));
        if self.answered.contains(&Some(run)) {
            return;
        }
        if let Some(current) = self.run.filter(|current| *current <= run) {
            if !self.stopping {
                self.stopping = true;
                self.discard();
            }
            self.settle();
            if current == run {
                return;
            }
        }
        let cursor = match self.retired {
            Some((retired, cursor)) if retired == run => cursor,
            _ => None,
        };
        self.answer(run, cursor);
    }

    /// The control pipe named `run`, its newest run: its frames may now start playing, and until
    /// then they're held, uncredited. A run playing already may finish; an older run never
    /// named last can't start.
    pub fn name(&mut self, run: RunId) {
        self.named = self.named.max(Some(run));
    }

    /// Closes admission to the device; [`Engine::quiet`] says `serial` once every content frame
    /// written before it has played. Unwritten speech stays queued.
    pub fn pause(&mut self, serial: u32) {
        self.pause = Some(serial);
        self.settle();
    }

    /// Withdraws pause `serial`, whether or not it went quiet: speech resumes at the first
    /// unwritten sample.
    pub fn resume(&mut self, serial: u32) {
        if self.pause == Some(serial) {
            self.pause = None;
        }
    }

    /// The latest pause whose written content has all played.
    pub fn quiet(&self) -> Option<u32> {
        self.quiet
    }

    fn queue(&mut self, segment: SegmentId) -> Result<(), ()> {
        if index(segment) != self.segments.len() || self.segments.len() >= max_segments() {
            return Err(());
        }
        self.segments.push(Segment::default());
        Ok(())
    }

    fn pcm(&mut self, pcm: &SpeechPcm) -> Result<(), ()> {
        pcm.check(self.rate).map_err(|_| ())?;
        let at = index(pcm.segment);
        let len = pcm.samples.len();
        let found = self.segments.get(at).ok_or(())?;
        let received = u32::try_from(len)
            .ok()
            .and_then(|len| found.received.checked_add(len))
            .filter(|&total| {
                u64::from(total) <= u64::from(self.rate) * u64::from(MAX_SEGMENT_SECONDS)
            })
            .ok_or(())?;
        if pcm.seq != found.chunks
            || found.last.is_some()
            || pcm.last.is_some_and(|total| total != received)
        {
            return Err(());
        }
        if len > 0 {
            let ahead = usize::try_from(MAX_AHEAD_SEGMENTS).map_err(|_| ())?;
            let unplayed = usize::try_from(self.clock.unplayed()).map_err(|_| ())?;
            // The arena plays in segment order, so a segment's audio waits for every earlier final.
            if at > self.reported + ahead
                || self.segments[..at]
                    .iter()
                    .any(|earlier| earlier.last.is_none())
                || self.queued + unplayed + len > self.arena.len()
            {
                return Err(());
            }
            let cap = self.arena.len();
            let mut to = (self.read + self.queued) % cap;
            let mut rest = pcm.samples.as_slice();
            while !rest.is_empty() {
                let n = rest.len().min(cap - to);
                self.arena[to..to + n].copy_from_slice(&rest[..n]);
                rest = &rest[n..];
                to = (to + n) % cap;
            }
            self.queued += len;
        }
        let found = &mut self.segments[at];
        found.received = received;
        found.chunks += 1;
        found.last = pcm.last;
        if pcm.last.is_some() {
            self.report();
        }
        Ok(())
    }

    /// The current run gives way to a newer one once it's fully played and reported.
    fn supersede(&mut self) -> bool {
        if self.stopping || self.queued > 0 || self.reported < self.segments.len() {
            return false;
        }
        if let Some(run) = self.run {
            self.retired = Some((run, self.cursor));
        }
        self.end_run();
        true
    }

    /// The next samples the staging segment can write: none while it waits for audio or for a
    /// queued segment at all. Steps past finished segments.
    fn available(&self, next: &mut Next) -> Option<usize> {
        loop {
            let segment = self.segments.get(next.staging)?;
            if segment.received > next.staged {
                return usize::try_from(segment.received - next.staged).ok();
            }
            if segment.last != Some(next.staged) {
                return None;
            }
            next.staging += 1;
            next.staged = 0;
        }
    }

    fn copy(&self, from: usize, out: &mut [i16]) {
        let first = out.len().min(self.arena.len() - from);
        out[..first].copy_from_slice(&self.arena[from..from + first]);
        let rest = out.len() - first;
        out[first..].copy_from_slice(&self.arena[..rest]);
    }

    /// Sends what played: each segment's `SegmentPlayed` in order once its total is known and
    /// played, then the playing segment's progress at most every 250 ms. Then settles.
    fn report(&mut self) {
        let Some(run) = self.run else {
            return;
        };
        while let Some(segment) = self.segments.get(self.reported) {
            if segment.last != Some(segment.played) {
                break;
            }
            let samples = segment.played;
            self.send(ControlToBrain::SegmentPlayed {
                run,
                segment: id(self.reported),
                samples,
            });
            advance(&mut self.cursor, self.reported, samples);
            self.reported += 1;
            self.progressed = 0;
        }
        let interval = scale(u64::from(PROGRESS_INTERVAL_MS), self.qpc_frequency, 1000);
        let now = self.clock.now();
        if let Some(segment) = self.segments.get(self.reported)
            && segment.played > self.progressed
            && self
                .progress_at
                .is_none_or(|at| now.saturating_sub(at) >= interval)
        {
            let sample = segment.played;
            self.progressed = sample;
            self.progress_at = Some(now);
            self.send(ControlToBrain::SpeechProgress {
                run,
                segment: id(self.reported),
                sample,
                t_qpc: now,
            });
        }
        self.settle();
    }

    /// Once every content frame written has played: a stopping run is answered and let go, and
    /// a pause is quiet.
    fn settle(&mut self) {
        if !self.clock.quiet() {
            return;
        }
        if self.stopping
            && let Some(run) = self.run
        {
            let cursor = self.cursor;
            self.answer(run, cursor);
            self.retired = Some((run, cursor));
            self.end_run();
        }
        if self.pause.is_some() {
            self.quiet = self.pause;
        }
    }

    fn answer(&mut self, run: RunId, cursor: Option<Cursor>) {
        self.answered[self.answers % ANSWERED] = Some(run);
        self.answers = self.answers.wrapping_add(1);
        self.send(ControlToBrain::SpeechStopped { run, cursor });
    }

    /// Queues a report. Unread progress gives way to newer progress; past the bound the brain
    /// isn't reading, and the speaker gives up rather than grow or drop a receipt.
    fn send(&mut self, report: ControlToBrain) {
        if matches!(report, ControlToBrain::SpeechProgress { .. })
            && let Some(last @ ControlToBrain::SpeechProgress { .. }) = self.reports.back_mut()
        {
            *last = report;
            return;
        }
        if self.reports.len() >= max_reports() {
            self.backlogged = true;
            self.fail(Fault::Backlog);
            return;
        }
        self.reports.push_back(report);
    }

    fn end_run(&mut self) {
        self.run = None;
        self.segments.clear();
        self.staging = 0;
        self.staged = 0;
        self.reported = 0;
        self.progressed = 0;
        self.progress_at = None;
        self.cursor = None;
        self.stopping = false;
    }

    /// Zeroes and lets go of every unwritten sample.
    fn discard(&mut self) {
        let cap = self.arena.len();
        while self.queued > 0 {
            let n = self.queued.min(cap - self.read);
            self.arena[self.read..self.read + n].fill(0);
            self.queued -= n;
            self.read = (self.read + n) % cap;
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.arena.as_mut().zeroize();
    }
}

fn advance(cursor: &mut Option<Cursor>, segment: usize, sample: u32) {
    let next = Cursor {
        segment: id(segment),
        sample,
    };
    if cursor.is_none_or(|old| (old.segment, old.sample) < (next.segment, next.sample)) {
        *cursor = Some(next);
    }
}

fn max_segments() -> usize {
    usize::try_from(MAX_SEGMENTS_PER_RUN).expect("256 fits in usize")
}

/// Reports the brain may leave unread: every segment's receipt and as many again, since
/// progress between two of them is coalesced.
fn max_reports() -> usize {
    2 * max_segments()
}

fn index(segment: SegmentId) -> usize {
    usize::try_from(segment.0).unwrap_or(usize::MAX)
}

fn id(at: usize) -> SegmentId {
    SegmentId(u32::try_from(at).expect("at most MAX_SEGMENTS_PER_RUN segments"))
}

fn wide(n: usize) -> u64 {
    u64::try_from(n).expect("a frame count fits in u64")
}

/// A sample count no larger than the arena, which holds 20 s at most.
fn count(n: usize) -> u32 {
    u32::try_from(n).expect("an arena count fits in u32")
}
