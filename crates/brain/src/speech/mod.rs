//! Speech input's worker process. sherpa-onnx and Parakeet run only in `kumochid --speech`, under
//! the shared supervisor, so a native exception, abort, crash or hang costs that worker and never
//! the brain. See `docs/development/voice.md`, "The speech worker".

mod supervisor;
use crate::platform::speech_wire as wire;
#[cfg(windows)]
mod worker;

pub use supervisor::{DEADLINES, Deadlines, SpeechHandle, SpeechSupervisor};
#[cfg(debug_assertions)]
pub use wire::SpeechFault;
pub use wire::{MAX_PCM_BYTES, MAX_RATE, MAX_SECONDS, MIN_RATE, NativeError, SpeechError};
#[cfg(windows)]
pub use worker::serve;

pub use crate::supervise::{Generation, WorkerLost};
