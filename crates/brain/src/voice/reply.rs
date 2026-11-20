//! One spoken run's speech: which segments exist, which one TTS is making, how much audio the
//! shell holds unplayed, and when the run's speech is final. No I/O: each call appends what the
//! coordinator must carry out to `out` as [`Speech`].
//!
//! - **The window.** A Speak goes out only while the unplayed audio is under
//!   [`MAX_UNPLAYED_SECONDS`] and at most [`MAX_AHEAD_SEGMENTS`] segments lie beyond the playing
//!   one. The pump reads TTS only on credit, which never lets forwarded audio pass the window;
//!   played progress gives credit back.
//! - **Every queued segment ends once:** with one `SpeechPcm` whose `last` carries the samples
//!   sent, empty when the audio already went or there was none.
//! - **Finality** (CAPS-165 H4): the producer closed, nothing is being spoken, and every surviving
//!   segment has played; after a revoke, the shell's `SpeechStopped` instead.

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;

use kumochi_protocol::message::{
    Cursor, MAX_AHEAD_SEGMENTS, MAX_SAY_BYTES, MAX_SEGMENTS_PER_RUN, MAX_UNPLAYED_SECONDS,
    OutputGeneration, RunId, SceneId, SegmentId, SegmentStart, SpeechPcm, SpeechPromise, StepRef,
};

use super::segment::{Cut, Segmenter};

/// What the coordinator carries out for a run's speech.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Speech {
    /// Tell the shell a segment will be spoken.
    Queued {
        segment: SegmentId,
        step: Option<StepRef>,
        start: SegmentStart,
        output_generation: OutputGeneration,
    },
    /// Hand TTS a segment; its events come back as [`Reply::pcm`], [`Reply::done`] and
    /// [`Reply::failed`].
    Speak { segment: SegmentId, text: String },
    /// Send the shell audio, or a segment's final chunk.
    Pcm(SpeechPcm),
    /// Let the pump read this many more samples from TTS.
    Credit(u64),
    /// Stop the current Speak and drop its receiver; the run's speech goes on.
    Cancel,
    /// Stop the run's speech for good: cancel TTS, drop the pump and, when `tell_shell`, send
    /// `SpeechRevoked`. The shell answers with `SpeechStopped` either way.
    Revoke { tell_shell: bool },
    /// (Re)arm the drain stall timer; only the latest epoch counts.
    Arm { epoch: u32 },
}

/// A speech report from the shell that can't be true. It carries no audio or text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refused {
    #[error("a speech report names no queued segment")]
    Unknown,
    #[error("a speech report is out of order")]
    OutOfOrder,
    #[error("a speech report doesn't match the samples sent")]
    Total,
}

/// What a TTS failure covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Only this segment; the next one may still speak.
    Segment,
    /// TTS can't speak the rest of the run either (not loaded, failed to load, gone).
    Run,
}

/// Who stops a run's speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revoker {
    /// The shell, with Stop: it's already stopping its output, so it isn't told.
    Shell,
    /// The brain: a drain stall, a device change, replacement, failure or quit.
    Brain,
}

/// How far the user heard: the segment, its samples played, and the byte offset in the answer
/// text where heard speech ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard {
    pub segment: SegmentId,
    pub sample: u32,
    pub text: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Synthesis {
    Open,
    /// TTS can't speak any more of this run; the rest of its segments end empty.
    Stopped,
    /// The run's speech was revoked; it's final once the shell is quiet.
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Open,
    /// Its final chunk went out.
    Last,
    Played,
    /// Its scene failed; nothing waits on it.
    Dropped,
}

enum Source {
    Text(Range<usize>),
    Say(StepRef),
}

struct Segment {
    source: Source,
    /// Until it's handed to TTS.
    text: Option<String>,
    sent: u32,
    played: u32,
    chunks: u32,
    end: End,
    /// Its final chunk went out; a dropped segment's waits for every earlier one's.
    marked: bool,
    /// TTS failed it, so playing all its audio isn't hearing all its text.
    failed: bool,
}

/// One spoken run's speech state, owned by the coordinator's run.
pub(crate) struct Reply {
    run: RunId,
    rate: u32,
    generation: OutputGeneration,
    segmenter: Segmenter,
    segments: Vec<Segment>,
    /// Samples sent and not yet reported played.
    unplayed: u64,
    /// Samples the pump may still read.
    credit: u64,
    speaking: Option<usize>,
    synthesis: Synthesis,
    /// The producer finished.
    closed: bool,
    /// The shell's `SpeechStopped` arrived after a revoke.
    quiet: bool,
    epoch: u32,
}

impl Reply {
    /// `rate` is the checked output rate from `AudioOutput`.
    pub(crate) fn new(run: RunId, rate: u32, generation: OutputGeneration) -> Self {
        Self {
            run,
            rate,
            generation,
            segmenter: Segmenter::default(),
            segments: Vec::new(),
            unplayed: 0,
            credit: 0,
            speaking: None,
            synthesis: Synthesis::Open,
            closed: false,
            quiet: false,
            epoch: 0,
        }
    }

    /// The output rate the run's speech is made at.
    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    /// Whether the run's speech was revoked.
    pub(crate) fn revoked(&self) -> bool {
        self.synthesis == Synthesis::Revoked
    }

    /// The engine's text delta.
    pub(crate) fn text(&mut self, delta: &str, out: &mut Vec<Speech>) {
        if self.closed {
            return;
        }
        let mut cuts = Vec::new();
        self.segmenter.push(delta, &mut cuts);
        self.add_cuts(cuts, out);
        self.pump(out);
    }

    /// Reserves the next segment id for a step about to commit. The caller holds later text
    /// until the step settles.
    pub(crate) fn reserve_say(&mut self, say: &str, out: &mut Vec<Speech>) -> Option<SegmentId> {
        let mut cuts = Vec::new();
        self.segmenter.flush(&mut cuts);
        self.add_cuts(cuts, out);
        let reserved = (self.synthesis == Synthesis::Open
            && self.segments.len() < max_segments()
            && say.len() <= MAX_SAY_BYTES
            && !say.trim().is_empty())
        .then(|| id(self.segments.len()));
        if reserved.is_none() {
            self.pump(out);
        }
        reserved
    }

    /// Binds a reserved segment to its durable scene before announcing it. A producer that
    /// stopped during the write still owes an empty final, unless the run was revoked.
    pub(crate) fn publish_say(
        &mut self,
        segment: SegmentId,
        step: StepRef,
        start: SegmentStart,
        say: &str,
        out: &mut Vec<Speech>,
    ) -> Option<SpeechPromise> {
        if self.synthesis == Synthesis::Revoked {
            return None;
        }
        assert_eq!(
            segment,
            id(self.segments.len()),
            "reply text must wait for the drawing receipt"
        );
        let published = self.append(Source::Say(step), start, say.to_owned(), out);
        if self.synthesis == Synthesis::Stopped {
            self.finals(out);
        }
        self.pump(out);
        (start == SegmentStart::Speech).then_some(published)
    }

    #[cfg(test)]
    pub(crate) fn say(&mut self, step: StepRef, say: &str, out: &mut Vec<Speech>) {
        if let Some(segment) = self.reserve_say(say, out) {
            let _ = self.publish_say(segment, step, SegmentStart::Speech, say, out);
        }
    }

    /// The producer finished: the tail becomes the last segment.
    pub(crate) fn close(&mut self, out: &mut Vec<Speech>) {
        if self.closed {
            return;
        }
        let mut cuts = Vec::new();
        self.segmenter.finish(&mut cuts);
        self.add_cuts(cuts, out);
        self.closed = true;
        self.pump(out);
        self.progressed(out);
    }

    /// Audio TTS made for `segment`, at the output rate. Audio for any other segment is stale.
    pub(crate) fn pcm(&mut self, segment: SegmentId, samples: Vec<i16>, out: &mut Vec<Speech>) {
        let Some(at) = self.speaking.filter(|&at| at == index(segment)) else {
            return;
        };
        if samples.is_empty() {
            return;
        }
        let most = usize::try_from(self.rate / 10).map_or(usize::MAX, |most| most.max(1));
        if samples.len() <= most {
            self.send(at, samples, out);
        } else {
            for chunk in samples.chunks(most) {
                self.send(at, chunk.to_vec(), out);
            }
        }
        self.progressed(out);
        self.pump(out);
    }

    /// TTS finished `segment`. Its final chunk carries the samples sent, which is what the shell
    /// received, whatever total TTS counted.
    pub(crate) fn done(&mut self, segment: SegmentId, out: &mut Vec<Speech>) {
        if self.speaking == Some(index(segment)) {
            self.end_speaking(out);
            self.pump(out);
        }
    }

    /// TTS failed `segment`, or its stream closed early. A [`Failure::Run`] ends every other
    /// open segment empty too.
    pub(crate) fn failed(&mut self, segment: SegmentId, failure: Failure, out: &mut Vec<Speech>) {
        if self.speaking != Some(index(segment)) {
            return;
        }
        if failure == Failure::Run {
            self.synthesis = Synthesis::Stopped;
        }
        self.segments[index(segment)].failed = true;
        self.end_speaking(out);
        self.pump(out);
    }

    /// `scene` failed: its say segments are dropped, and audio already sent for them stops
    /// counting against the window. Once the run's speech is revoked nothing more goes out.
    pub(crate) fn step_failed(&mut self, scene: SceneId, out: &mut Vec<Speech>) {
        if self.synthesis == Synthesis::Revoked {
            return;
        }
        let mut changed = false;
        for at in 0..self.segments.len() {
            let segment = &mut self.segments[at];
            let Source::Say(step) = segment.source else {
                continue;
            };
            if step.scene != scene || matches!(segment.end, End::Played | End::Dropped) {
                continue;
            }
            let unplayed = u64::from(segment.sent - segment.played);
            segment.end = End::Dropped;
            segment.text = None;
            self.unplayed -= unplayed;
            changed = true;
            if self.speaking == Some(at) {
                self.speaking = None;
                out.push(Speech::Cancel);
            }
        }
        if changed {
            self.finals(out);
            self.progressed(out);
            self.pump(out);
        }
    }

    /// The shell played `sample` samples of `segment`, which must be the one playing.
    ///
    /// # Errors
    /// [`Refused`] when the report can't be true; a stale or repeated one is ignored.
    pub(crate) fn progress(
        &mut self,
        segment: SegmentId,
        sample: u32,
        out: &mut Vec<Speech>,
    ) -> Result<(), Refused> {
        if self.quiet {
            return Ok(());
        }
        let Some(at) = self.playing_report(segment)? else {
            return Ok(());
        };
        let found = &mut self.segments[at];
        if sample > found.sent {
            return Err(Refused::Total);
        }
        if sample <= found.played {
            return Ok(());
        }
        self.unplayed -= u64::from(sample - found.played);
        found.played = sample;
        self.progressed(out);
        self.pump(out);
        Ok(())
    }

    /// The shell played the last of `segment`'s `samples`, which must be its total.
    ///
    /// # Errors
    /// [`Refused`] when the segment isn't the playing one, hasn't ended, or played a different
    /// total.
    pub(crate) fn played(
        &mut self,
        segment: SegmentId,
        samples: u32,
        out: &mut Vec<Speech>,
    ) -> Result<(), Refused> {
        if self.quiet {
            return Ok(());
        }
        let Some(at) = self.playing_report(segment)? else {
            return Ok(());
        };
        let found = &mut self.segments[at];
        if found.end != End::Last {
            return Err(Refused::OutOfOrder);
        }
        if samples != found.sent {
            return Err(Refused::Total);
        }
        self.unplayed -= u64::from(found.sent - found.played);
        found.played = found.sent;
        found.end = End::Played;
        self.progressed(out);
        self.pump(out);
        Ok(())
    }

    /// The output device's generation. A new one mid-run stops the run's speech; its text stays.
    /// shortcut: a device change ends the run's speech; replay at the new rate once progress and
    /// heard cursors carry an attempt identity.
    pub(crate) fn output(&mut self, generation: OutputGeneration, out: &mut Vec<Speech>) {
        if generation != self.generation {
            self.revoke(Revoker::Brain, out);
        }
    }

    /// The output device of `generation` is gone: a reply made for it stops speaking; its text
    /// stays.
    pub(crate) fn withdrawn(&mut self, generation: OutputGeneration, out: &mut Vec<Speech>) {
        if generation == self.generation {
            self.revoke(Revoker::Brain, out);
        }
    }

    /// Stops the run's speech for good.
    pub(crate) fn revoke(&mut self, by: Revoker, out: &mut Vec<Speech>) {
        if self.synthesis == Synthesis::Revoked {
            return;
        }
        self.synthesis = Synthesis::Revoked;
        self.speaking = None;
        self.credit = 0;
        out.push(Speech::Revoke {
            tell_shell: by == Revoker::Brain,
        });
    }

    /// The shell's quiet answer after a revoke, with where playback stopped. It's the last word
    /// on the heard point: later progress and played reports change nothing.
    ///
    /// # Errors
    /// [`Refused`] when nothing was revoked, the answer came twice, or the cursor can't be true:
    /// past a segment whose audio hadn't all gone, or behind what already played.
    pub(crate) fn stopped(&mut self, cursor: Option<Cursor>) -> Result<(), Refused> {
        if self.synthesis != Synthesis::Revoked || self.quiet {
            return Err(Refused::OutOfOrder);
        }
        if let Some(cursor) = cursor {
            let at = index(cursor.segment);
            let found = self.segments.get(at).ok_or(Refused::Unknown)?;
            if cursor.sample > found.sent {
                return Err(Refused::Total);
            }
            let (earlier, later) = (&self.segments[..at], &self.segments[at + 1..]);
            if cursor.sample < found.played
                || earlier.iter().any(|segment| segment.end == End::Open)
                || later
                    .iter()
                    .any(|segment| segment.played > 0 || segment.end == End::Played)
            {
                return Err(Refused::OutOfOrder);
            }
            for earlier in &mut self.segments[..at] {
                if earlier.end == End::Last {
                    earlier.played = earlier.sent;
                    earlier.end = End::Played;
                }
            }
            let found = &mut self.segments[at];
            found.played = cursor.sample;
            if found.end == End::Last && cursor.sample == found.sent {
                found.end = End::Played;
            }
        }
        self.quiet = true;
        Ok(())
    }

    /// Whether the run's speech is final (CAPS-165 H4).
    pub(crate) fn is_final(&self) -> bool {
        match self.synthesis {
            Synthesis::Revoked => self.quiet,
            Synthesis::Open | Synthesis::Stopped => {
                self.closed
                    && self.speaking.is_none()
                    && self
                        .segments
                        .iter()
                        .all(|segment| matches!(segment.end, End::Played | End::Dropped))
            }
        }
    }

    /// Whether the stall timer armed with `epoch` should revoke: it's the latest one, and the
    /// speech is still draining.
    pub(crate) fn stalled(&self, epoch: u32) -> bool {
        epoch == self.epoch && self.synthesis != Synthesis::Revoked && !self.is_final()
    }

    /// The furthest point the user heard, if any audio played.
    pub(crate) fn heard(&self) -> Option<Heard> {
        let mut text = 0;
        let mut heard = None;
        for (at, segment) in self.segments.iter().enumerate() {
            if segment.played == 0 {
                continue;
            }
            if let Source::Text(range) = &segment.source {
                text = if segment.end == End::Played && !segment.failed {
                    range.end
                } else {
                    text.max(range.start)
                };
            }
            heard = Some(Heard {
                segment: id(at),
                sample: segment.played,
                text,
            });
        }
        heard
    }

    fn add_cuts(&mut self, cuts: Vec<Cut>, out: &mut Vec<Speech>) {
        for cut in cuts {
            self.add(Source::Text(cut.range), cut.text, out);
        }
    }

    /// Past [`MAX_SEGMENTS_PER_RUN`], or once synthesis stopped, the rest stays text.
    fn add(&mut self, source: Source, text: String, out: &mut Vec<Speech>) {
        if self.synthesis != Synthesis::Open || self.segments.len() >= max_segments() {
            return;
        }
        self.append(source, SegmentStart::Plain, text, out);
    }

    fn append(
        &mut self,
        source: Source,
        start: SegmentStart,
        text: String,
        out: &mut Vec<Speech>,
    ) -> SpeechPromise {
        let step = match source {
            Source::Say(step) => Some(step),
            Source::Text(_) => None,
        };
        let segment = id(self.segments.len());
        let output_generation = self.generation;
        out.push(Speech::Queued {
            segment,
            step,
            start,
            output_generation,
        });
        self.segments.push(Segment {
            source,
            text: Some(text),
            sent: 0,
            played: 0,
            chunks: 0,
            end: End::Open,
            marked: false,
            failed: false,
        });
        SpeechPromise {
            output_generation,
            segment,
        }
    }

    /// Issues the next Speak when the window allows, and tops up the pump's credit.
    fn pump(&mut self, out: &mut Vec<Speech>) {
        if self.synthesis != Synthesis::Open {
            return;
        }
        let window = u64::from(MAX_UNPLAYED_SECONDS) * u64::from(self.rate);
        if self.speaking.is_none() && self.unplayed < window {
            let playing = self.playing();
            let next = self
                .segments
                .iter()
                .position(|segment| segment.end == End::Open && segment.text.is_some());
            if let Some(next) = next.filter(|&next| next - playing <= max_ahead())
                && let Some(text) = self.segments[next].text.take()
            {
                self.speaking = Some(next);
                out.push(Speech::Speak {
                    segment: id(next),
                    text,
                });
                self.progressed(out);
            }
        }
        let room = window.saturating_sub(self.unplayed + self.credit);
        if self.speaking.is_some() && room > 0 {
            self.credit += room;
            out.push(Speech::Credit(room));
        }
    }

    /// The first segment that hasn't played or been dropped.
    fn playing(&self) -> usize {
        self.segments
            .iter()
            .position(|segment| !matches!(segment.end, End::Played | End::Dropped))
            .unwrap_or(self.segments.len())
    }

    /// The segment a progress or played report may name: the playing one. `None` for a
    /// dropped segment, whose late reports mean nothing.
    fn playing_report(&self, segment: SegmentId) -> Result<Option<usize>, Refused> {
        let at = index(segment);
        let found = self.segments.get(at).ok_or(Refused::Unknown)?;
        match found.end {
            End::Dropped => Ok(None),
            End::Played => Err(Refused::OutOfOrder),
            End::Open | End::Last if at != self.playing() => Err(Refused::OutOfOrder),
            End::Open | End::Last => Ok(Some(at)),
        }
    }

    fn send(&mut self, at: usize, samples: Vec<i16>, out: &mut Vec<Speech>) {
        let len = u32::try_from(samples.len()).expect("a speech chunk is at most rate / 10");
        let segment = &mut self.segments[at];
        out.push(Speech::Pcm(SpeechPcm {
            run: self.run,
            segment: id(at),
            output_generation: self.generation,
            seq: segment.chunks,
            samples,
            last: None,
        }));
        segment.chunks += 1;
        segment.sent += len;
        self.unplayed += u64::from(len);
        self.credit = self.credit.saturating_sub(u64::from(len));
    }

    fn end_speaking(&mut self, out: &mut Vec<Speech>) {
        if let Some(at) = self.speaking.take() {
            self.end(at, out);
            self.finals(out);
            self.progressed(out);
        }
    }

    /// Sends the final chunks owed, in segment order: a dropped segment's, or once synthesis
    /// stopped an open one's, waits for every earlier segment's.
    fn finals(&mut self, out: &mut Vec<Speech>) {
        for at in 0..self.segments.len() {
            let segment = &self.segments[at];
            if segment.marked {
                continue;
            }
            let owed = segment.end == End::Dropped
                || self.synthesis == Synthesis::Stopped && segment.end == End::Open;
            if !owed || self.speaking == Some(at) {
                return;
            }
            self.end(at, out);
        }
    }

    /// Sends `at`'s final chunk: empty, with the samples sent as its total.
    fn end(&mut self, at: usize, out: &mut Vec<Speech>) {
        let segment = &mut self.segments[at];
        out.push(Speech::Pcm(SpeechPcm {
            run: self.run,
            segment: id(at),
            output_generation: self.generation,
            seq: segment.chunks,
            samples: Vec::new(),
            last: Some(segment.sent),
        }));
        segment.chunks += 1;
        segment.text = None;
        segment.marked = true;
        if segment.end == End::Open {
            segment.end = End::Last;
        }
    }

    /// Anything that moves a draining run along re-arms its stall timer.
    fn progressed(&mut self, out: &mut Vec<Speech>) {
        if self.closed && self.synthesis != Synthesis::Revoked && !self.is_final() {
            self.epoch = self.epoch.wrapping_add(1);
            out.push(Speech::Arm { epoch: self.epoch });
        }
    }
}

fn index(segment: SegmentId) -> usize {
    usize::try_from(segment.0).unwrap_or(usize::MAX)
}

fn id(at: usize) -> SegmentId {
    SegmentId(u32::try_from(at).expect("at most MAX_SEGMENTS_PER_RUN segments"))
}

fn max_segments() -> usize {
    usize::try_from(MAX_SEGMENTS_PER_RUN).unwrap_or(usize::MAX)
}

fn max_ahead() -> usize {
    usize::try_from(MAX_AHEAD_SEGMENTS).unwrap_or(usize::MAX)
}

impl fmt::Debug for Speech {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued {
                segment,
                step,
                start,
                output_generation,
            } => f
                .debug_struct("Queued")
                .field("segment", segment)
                .field("step", step)
                .field("start", start)
                .field("output_generation", output_generation)
                .finish(),
            Self::Speak { segment, text } => f
                .debug_struct("Speak")
                .field("segment", segment)
                .field("text", &text.len())
                .finish(),
            Self::Pcm(pcm) => f.debug_tuple("Pcm").field(pcm).finish(),
            Self::Credit(samples) => f.debug_tuple("Credit").field(samples).finish(),
            Self::Cancel => f.write_str("Cancel"),
            Self::Revoke { tell_shell } => f
                .debug_struct("Revoke")
                .field("tell_shell", tell_shell)
                .finish(),
            Self::Arm { epoch } => f.debug_struct("Arm").field("epoch", epoch).finish(),
        }
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("run", &self.run)
            .field("segmenter", &self.segmenter)
            .field("segments", &self.segments.len())
            .field("unplayed", &self.unplayed)
            .field("credit", &self.credit)
            .field("speaking", &self.speaking)
            .field("synthesis", &self.synthesis)
            .field("closed", &self.closed)
            .field("quiet", &self.quiet)
            .finish_non_exhaustive()
    }
}
