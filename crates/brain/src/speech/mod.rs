//! Speech input's worker process. sherpa-onnx and Parakeet run only in `kumochid --speech`, under
//! the shared supervisor, so a native exception, abort, crash or hang costs that worker and never
//! the brain. See `docs/development/voice.md`, "The speech worker".

mod supervisor;
use crate::platform::speech_wire as wire;
#[cfg(windows)]
pub(crate) mod worker;

use kumochi_protocol::frame::FrameError;

use crate::platform::{self, Mode};

pub use crate::supervise::{Generation, WorkerLost};
pub use supervisor::{DEADLINES, Deadlines, SpeechHandle, SpeechSupervisor};
#[cfg(debug_assertions)]
pub use wire::SpeechFault;
pub use wire::{MAX_PCM_BYTES, MAX_RATE, MAX_SECONDS, MIN_RATE, NativeError, SpeechError};

/// Serves stdin/stdout after the caller initializes content-free logging.
///
/// # Errors
/// Returns an unsupported-runtime, framing or transport failure, never request contents.
pub fn serve() -> Result<(), FrameError> {
    platform::require_mode(Mode::Detached)?;
    platform::serve_speech()
}
