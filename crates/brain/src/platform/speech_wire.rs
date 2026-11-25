use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::supervise::WorkerLost;

/// The lowest capture rate accepted, in Hz [guess].
pub const MIN_RATE: u32 = 8_000;
/// The highest capture rate accepted, in Hz [guess].
pub const MAX_RATE: u32 = 48_000;
/// The longest utterance, at any rate [guess].
pub const MAX_SECONDS: u32 = 60;
/// The most PCM one decode carries: [`MAX_SECONDS`] at [`MAX_RATE`], 16-bit.
pub const MAX_PCM_BYTES: usize = 5_760_000;

/// Why speech input's native side failed. It carries no native text and no paths.
#[derive(
    Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub enum NativeError {
    /// A native library is missing.
    #[error("a native library is missing")]
    Missing,
    /// A folder or native library on the load path is a reparse point or resolves elsewhere.
    #[error("a folder or native library on the load path is a reparse point or resolves elsewhere")]
    Reparse,
    /// Someone other than this user or the system can change the load path.
    #[error("someone other than this user or the system can change the load path")]
    Unsafe,
    /// A native library doesn't match its pin.
    #[error("a native library doesn't match its pin")]
    Pin,
    /// A module the native libraries import is already loaded from another folder.
    #[error("a module the native libraries import is already loaded from another folder")]
    Conflict,
    /// A system library the native libraries need is missing.
    #[error("a system library the native libraries need is missing")]
    Dependency,
    /// Loading brought in a module from an unexpected folder.
    #[error("loading brought in a module from an unexpected folder")]
    Unexpected,
    /// A native library failed to load.
    #[error("a native library failed to load")]
    Load,
    /// A native library lacks an export.
    #[error("a native library lacks an export")]
    Symbol,
    /// The native library isn't the pinned release.
    #[error("the native library isn't the pinned release")]
    Abi,
    /// The recognizer or a stream couldn't be created.
    #[error("the recognizer or a stream couldn't be created")]
    Create,
    /// The audio can't be handed to the recognizer.
    #[error("the audio can't be handed to the recognizer")]
    Input,
    /// Decoding gave no result.
    #[error("decoding gave no result")]
    Decode,
    /// The transcript is too long or isn't UTF-8.
    #[error("the transcript is too long or isn't UTF-8")]
    Text,
    /// The model folder or a model file isn't an ordinary trusted local path.
    #[error("the speech model path is unsafe")]
    ModelPath,
}

/// Why speech input didn't answer. No native text and no paths cross this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpeechError {
    /// The worker died, broke its protocol or missed a deadline; this does not prove its exit.
    #[error("speech worker lost")]
    WorkerLost,
    /// A lost worker wasn't seen to exit, so nothing replaces it and speech input stays refused.
    #[error("speech worker exit unconfirmed")]
    ExitUnconfirmed,
    /// The worker has no model loaded.
    #[error("no speech model is loaded")]
    NotLoaded,
    /// The audio's rate or length is out of bounds; nothing was sent.
    #[error("the audio's rate or length is out of bounds")]
    Input,
    /// The worker's native side failed.
    #[error(transparent)]
    Native(#[from] NativeError),
}

impl From<WorkerLost> for SpeechError {
    fn from(lost: WorkerLost) -> Self {
        match lost {
            WorkerLost::Lost => Self::WorkerLost,
            WorkerLost::ExitUnconfirmed => Self::ExitUnconfirmed,
        }
    }
}

/// Checks a capture rate and a sample count against the rate bounds, the byte ceiling and the
/// duration ceiling. Every duration computed later relies on this having passed.
pub(crate) fn check_audio(rate: u32, samples: usize) -> Result<(), SpeechError> {
    let longest = u64::from(rate) * u64::from(MAX_SECONDS);
    let fits = (MIN_RATE..=MAX_RATE).contains(&rate)
        && samples > 0
        && samples
            .checked_mul(2)
            .is_some_and(|bytes| bytes <= MAX_PCM_BYTES)
        && u64::try_from(samples).is_ok_and(|samples| samples <= longest);
    if fits {
        Ok(())
    } else {
        Err(SpeechError::Input)
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum SpeechRequest {
    /// Stage the natives from `natives`, load them, create Parakeet's recognizer from `models`
    /// and warm it up. Any model already loaded is dropped first.
    Load { natives: PathBuf, models: PathBuf },
    /// Transcribe mono 16-bit little-endian PCM at `rate`.
    Decode { rate: u32, pcm: Vec<u8> },
    /// Drop the recognizer and the natives.
    Unload,
    #[cfg(debug_assertions)]
    Fault(SpeechFault),
}

impl std::fmt::Debug for SpeechRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpeechRequest(***)")
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SpeechReply {
    Loaded,
    Text(String),
    Unloaded,
    NotLoaded,
    Failed(NativeError),
}

impl std::fmt::Debug for SpeechReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loaded => f.write_str("Loaded"),
            Self::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Self::Unloaded => f.write_str("Unloaded"),
            Self::NotLoaded => f.write_str("NotLoaded"),
            Self::Failed(error) => write!(f, "Failed({error:?})"),
        }
    }
}

/// A dev-only, one-request failure fixture, raised inside the worker's native boundary.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SpeechFault {
    /// Stop answering.
    Hang,
    /// Exit abnormally.
    Crash,
    /// Abort, as `std::terminate` does.
    Abort,
    /// Raise a noncontinuable access violation through Windows.
    AccessViolation,
    /// Write an invalid frame.
    Garbage,
    /// Panic after installing the location-only hook.
    Panic,
    /// Call `kumochi_test_throw` in the DLL at this path, which throws a C++ exception. If the DLL
    /// doesn't load the reply is `Failed(Load)`, and if the call returns it's `Failed(Decode)`.
    Throw(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::{MAX_PCM_BYTES, SpeechError, SpeechReply, SpeechRequest, check_audio};

    #[test]
    fn audio_must_fit_the_rate_bounds_and_both_ceilings() {
        assert_eq!(check_audio(16_000, 16_000 * 60), Ok(()));
        assert_eq!(check_audio(48_000, MAX_PCM_BYTES / 2), Ok(()));
        for (rate, samples) in [
            (16_000, 0),
            (7_999, 100),
            (48_001, 100),
            (16_000, 16_000 * 60 + 1),
            (8_000, 8_000 * 60 + 1),
            (48_000, MAX_PCM_BYTES / 2 + 1),
            (16_000, usize::MAX),
        ] {
            assert_eq!(
                check_audio(rate, samples),
                Err(SpeechError::Input),
                "{rate} {samples}"
            );
        }
    }

    #[test]
    fn debug_shows_no_audio_text_or_paths() {
        let request = SpeechRequest::Load {
            natives: "C:\\private\\natives".into(),
            models: "C:\\private\\models".into(),
        };
        assert_eq!(format!("{request:?}"), "SpeechRequest(***)");
        let decode = SpeechRequest::Decode {
            rate: 16_000,
            pcm: vec![7; 4],
        };
        assert_eq!(format!("{decode:?}"), "SpeechRequest(***)");
        let text = SpeechReply::Text("a private sentence".into());
        assert_eq!(format!("{text:?}"), "Text(18 bytes)");
    }
}
