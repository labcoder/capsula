//! Voice on the pipes: microphone audio for a hold, the transcript, and spoken replies as
//! segments of PCM. The bounds here are the only copies; the brain and the shell import them.
//! Audio and text print only their lengths in `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{GestureId, RunId, SceneId};
use crate::frame::Pipe;

/// The lowest microphone or output rate, in Hz [guess].
pub const MIN_RATE: u32 = 8_000;
/// The highest microphone or output rate, in Hz [guess].
pub const MAX_RATE: u32 = 192_000;
/// The most chunks one hold sends: 60 s of 20 ms chunks [guess].
pub const MAX_HOLD_CHUNKS: u32 = 3_000;
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
