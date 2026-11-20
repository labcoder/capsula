//! Voice on the pipes: microphone audio for a hold, the transcript, and spoken replies as
//! segments of PCM. The bounds here are the only copies; the brain and the shell import them.
//! Audio and text print only their lengths in `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ErrorKey, GestureId, RunId, SceneId};
use crate::frame::Pipe;

/// The lowest microphone or output rate, in Hz [guess].
pub const MIN_RATE: u32 = 8_000;
/// The highest microphone or output rate, in Hz [guess].
pub const MAX_RATE: u32 = 192_000;
/// The most chunks one hold sends: 60 s of 20 ms chunks [guess].
pub const MAX_HOLD_CHUNKS: u32 = 3_000;
/// Unmeasured open/start upper bound, generous for Bluetooth; revisit after native review.
pub const HOLD_START_TIMEOUT_MS: u32 = 10_000;
/// Unmeasured final-audio drain upper bound; revisit after native review, never a stop receipt.
pub const HOLD_DRAIN_TIMEOUT_MS: u32 = 2_000;
/// The longest single segment [guess].
pub const MAX_SEGMENT_SECONDS: u32 = 30;
/// Queued but unplayed audio across a run, on both sides [guess].
pub const MAX_UNPLAYED_SECONDS: u32 = 20;
/// Segments beyond the one playing [guess].
pub const MAX_AHEAD_SEGMENTS: u32 = 2;
/// Segments one run may speak; past this the rest stays text [guess].
pub const MAX_SEGMENTS_PER_RUN: u32 = 256;
/// The longest step `say` that's spoken, in bytes [guess].
pub const MAX_SAY_BYTES: usize = 1_024;
/// The longest transcript, in bytes [guess].
pub const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024;
/// The shortest gap between two `SpeechProgress` reports, in milliseconds [guess].
pub const PROGRESS_INTERVAL_MS: u32 = 250;

/// The largest microphone chunk: 20 ms at the highest rate.
pub const MAX_MIC_SAMPLES: usize = 3_840;
/// The largest speech chunk: 100 ms at the highest rate.
pub const MAX_SPEECH_SAMPLES: usize = 19_200;

// A sample is at most three bytes as a postcard varint, plus a little framing: the largest
// chunks fit their pipe.
const _: () = assert!(
    MAX_MIC_SAMPLES * 3 + 64 <= Pipe::Data.max_payload(),
    "a microphone chunk fits a data frame"
);
const _: () = assert!(
    MAX_SPEECH_SAMPLES * 3 + 64 <= Pipe::Data.max_payload(),
    "a speech chunk fits a data frame"
);
const _: () = assert!(
    MAX_TRANSCRIPT_BYTES + 64 <= Pipe::Data.max_payload(),
    "a transcript fits a data frame"
);

/// A spoken segment of one run, numbered from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub u32);

/// The shell's output device generation; PCM resampled for another one is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputGeneration(pub u32);

/// Whether a queued segment narrates a step authored to start with speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentStart {
    /// Ordinary text speech, or narration for a step with an independent start.
    Plain,
    /// Narration for a step authored to start with speech.
    Speech,
}

/// The exact live narration promised by a scene commit, within the commit's run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechPromise {
    /// The logical output generation used by the announcement and PCM.
    pub output_generation: OutputGeneration,
    /// The matching segment in the run.
    pub segment: SegmentId,
}

/// One voice-only hold's admission or terminal outcome; never audio or recognized words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldStatus {
    /// The exact hold may open input; collection still waits for its running/UI receipt.
    Admitted,
    /// Local speech input is loading; there is no recording permit yet.
    Loading,
    /// Counted audio and a physical source-stop receipt admitted local decoding.
    Transcribing,
    /// No recognized words; nothing was stored or sent.
    Empty,
    /// The input was terminally cancelled.
    Cancelled,
    /// The input failed; later data cannot revive it.
    Failed(HoldFailure),
}

/// Fixed voice-input failures, safe in status frames, errors and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldFailure {
    /// Speech input is not installed or loaded.
    NotLoaded,
    /// Another input or decode still owns the bounded slot.
    Busy,
    /// An owner or its cleanup is unavailable; a restart may be needed.
    Unavailable,
    /// The framing, rate, sequence or final count disagrees.
    InvalidAudio,
    /// The hold exceeded the existing audio bound.
    TooLong,
    /// An existing hold or decode deadline ended the input.
    TimedOut,
    /// The pinned target, model or connection no longer matches.
    ChoiceChanged,
    /// Local speech recognition failed without recognized text.
    DecodeFailed,
}

/// A committed step: its scene and its lifetime index there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    /// The scene.
    pub scene: SceneId,
    /// The step's zero-based lifetime index.
    pub index: u32,
}

/// Where the shell's output stopped: the segment and the samples of it that played.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The segment.
    pub segment: SegmentId,
    /// Samples of it played.
    pub sample: u32,
}

/// Ordered source retirement on a queued output, independent of its physical routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechRetirement {
    /// The spoken run.
    pub run: RunId,
    /// The logical output that accepted its source.
    pub output: OutputGeneration,
    /// The segment whose source retired.
    pub segment: SegmentId,
    /// Cumulative retired samples within that segment.
    pub retired: u32,
    /// Samples within this segment covered by the run's contiguous heard prefix.
    pub heard: u32,
    /// The validated final packet arrived and all its samples retired.
    pub final_segment: bool,
}

/// Each voice part's state, as the brain's installer reports it. Nothing sends it until the
/// installer lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceView {
    /// Speech to text.
    pub speech_input: PartState,
    /// Text to speech.
    pub spoken_replies: PartState,
}

/// One voice part's state. Append only: the tags travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartState {
    /// Not on disk.
    NotInstalled,
    /// Checking files already on disk.
    Verifying,
    /// Downloading: bytes so far of the part's total.
    Downloading {
        /// Bytes on disk and checked so far.
        done: u64,
        /// The part's installed size.
        total: u64,
    },
    /// Installed and loading.
    Loading,
    /// Loaded and ready.
    Ready,
    /// Installed, and unloaded while idle.
    Unloaded,
    /// The last install, load or remove failed.
    Failed {
        /// Why.
        key: ErrorKey,
        /// Whether the part is still installed.
        installed: bool,
    },
    /// The owner stopped without confirmed release; restart is required.
    Unavailable,
}

/// Why a voice frame was refused. It carries no audio or text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VoiceBound {
    /// A rate outside [`MIN_RATE`] to [`MAX_RATE`].
    #[error("the rate is outside 8 to 192 kHz")]
    Rate,
    /// A chunk with the wrong number of samples.
    #[error("the chunk has the wrong number of samples")]
    Samples,
    /// A hold past [`MAX_HOLD_CHUNKS`].
    #[error("the hold sent more chunks than it may")]
    Chunks,
    /// A segment past [`MAX_SEGMENT_SECONDS`].
    #[error("the segment is longer than allowed")]
    Segment,
    /// Text past its byte limit.
    #[error("the text is longer than allowed")]
    Text,
}

/// Whether `rate` is a rate either side may use.
///
/// # Errors
/// `Rate` outside [`MIN_RATE`] to [`MAX_RATE`].
pub const fn check_rate(rate: u32) -> Result<(), VoiceBound> {
    if rate < MIN_RATE || rate > MAX_RATE {
        return Err(VoiceBound::Rate);
    }
    Ok(())
}

/// 20 ms of a hold's microphone audio, mono 16-bit at the device's rate.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicPcm {
    /// The hold.
    pub gesture: GestureId,
    /// Counted from 0 within the hold.
    pub seq: u32,
    /// The capture rate.
    pub rate: u32,
    /// The hold's last chunk.
    pub last: bool,
    /// Exactly `rate / 50` samples, or 1 to that many when `last` is set.
    pub samples: Vec<i16>,
}

impl MicPcm {
    /// Checks the rate, the sequence number and the chunk's length.
    ///
    /// # Errors
    /// The bound the chunk breaks.
    pub fn check(&self) -> Result<(), VoiceBound> {
        check_rate(self.rate)?;
        if self.seq >= MAX_HOLD_CHUNKS {
            return Err(VoiceBound::Chunks);
        }
        let full = usize::try_from(self.rate / 50).map_err(|_| VoiceBound::Rate)?;
        let fits = if self.last {
            (1..=full).contains(&self.samples.len())
        } else {
            self.samples.len() == full
        };
        if !fits {
            return Err(VoiceBound::Samples);
        }
        Ok(())
    }
}

impl fmt::Debug for MicPcm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MicPcm")
            .field("gesture", &self.gesture)
            .field("seq", &self.seq)
            .field("rate", &self.rate)
            .field("last", &self.last)
            .field("samples", &self.samples.len())
            .finish()
    }
}

/// Up to 100 ms of one spoken segment, mono 16-bit at the output's rate.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechPcm {
    /// The run.
    pub run: RunId,
    /// The segment.
    pub segment: SegmentId,
    /// The output generation the samples were resampled for.
    pub output_generation: OutputGeneration,
    /// Counted from 0 within the segment.
    pub seq: u32,
    /// 1 to `rate / 10` samples; none only on a final chunk, which ends a segment whose audio
    /// already went or that has none.
    pub samples: Vec<i16>,
    /// On the segment's final chunk: the segment's total samples.
    pub last: Option<u32>,
}

impl SpeechPcm {
    /// Checks the chunk against the output's `rate`.
    ///
    /// # Errors
    /// The bound the chunk breaks.
    pub fn check(&self, rate: u32) -> Result<(), VoiceBound> {
        check_rate(rate)?;
        let most = usize::try_from(rate / 10).map_err(|_| VoiceBound::Rate)?;
        let least = usize::from(self.last.is_none());
        if !(least..=most).contains(&self.samples.len()) {
            return Err(VoiceBound::Samples);
        }
        if self.last.is_some_and(|total| {
            u64::from(total) > u64::from(rate) * u64::from(MAX_SEGMENT_SECONDS)
                || usize::try_from(total).is_ok_and(|total| total < self.samples.len())
        }) {
            return Err(VoiceBound::Segment);
        }
        Ok(())
    }
}

impl fmt::Debug for SpeechPcm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeechPcm")
            .field("run", &self.run)
            .field("segment", &self.segment)
            .field("output_generation", &self.output_generation)
            .field("seq", &self.seq)
            .field("samples", &self.samples.len())
            .field("last", &self.last)
            .finish()
    }
}

/// What a hold said, as the turn's text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    /// The run the hold started.
    pub run: RunId,
    /// At most [`MAX_TRANSCRIPT_BYTES`].
    pub text: String,
}

impl Transcript {
    /// Checks the text's length.
    ///
    /// # Errors
    /// `Text` past [`MAX_TRANSCRIPT_BYTES`].
    pub fn check(&self) -> Result<(), VoiceBound> {
        if self.text.len() > MAX_TRANSCRIPT_BYTES {
            return Err(VoiceBound::Text);
        }
        Ok(())
    }
}

impl fmt::Debug for Transcript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcript")
            .field("run", &self.run)
            .field("text", &self.text.len())
            .finish()
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
