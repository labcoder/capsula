use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use kumochi_protocol::frame::FrameError;

use super::supervisor::Speech;
use super::wire::{SpeechReply, SpeechRequest, check_audio};
use crate::platform::sherpa::{NativeError, Recognizer, Sherpa, c_path, staging_root};
use crate::supervise::{Kind as _, read_request, write_message};

/// The warm-up decode at load: one second of silence at 16 kHz [S5].
const PREWARM_RATE: u32 = 16_000;
static PREWARM: [i16; 16_000] = [0; 16_000];

type Next = Option<(u64, SpeechRequest)>;

/// Serves stdin/stdout after the caller initializes content-free logging.
///
/// Every native call runs inside one fail-stop boundary. sherpa-onnx's functions are declared
/// `C-unwind`, so a C++ exception that escapes one unwinds into Rust in a defined way; a release
/// build (`panic = "abort"`) aborts there, and a debug build either aborts or lands here. Here
/// the process aborts at once, without looking at or dropping what unwound and without serving
/// another request on that native state. A Rust panic ends the same way, after the location-only
/// panic hook logs where.
///
/// # Errors
/// Returns a framing or transport failure, never request contents.
pub fn serve() -> Result<(), FrameError> {
    crate::platform::quiet_failures();
    match std::panic::catch_unwind(AssertUnwindSafe(serve_requests)) {
        Ok(served) => served,
        Err(_) => std::process::abort(),
    }
}

fn serve_requests() -> Result<(), FrameError> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut next = read(&mut input)?;
    while let Some((id, request)) = next {
        next = match request {
            SpeechRequest::Load { natives, models } => {
                loaded(&mut input, &mut output, id, &natives, &models, staging_root)?
            }
            SpeechRequest::Decode { .. } => {
                reply(&mut output, id, &SpeechReply::NotLoaded)?;
                read(&mut input)?
            }
            SpeechRequest::Unload => {
                reply(&mut output, id, &SpeechReply::Unloaded)?;
                read(&mut input)?
            }
            #[cfg(debug_assertions)]
            SpeechRequest::Fault(fault) => {
                inject(fault, &mut output, id)?;
                read(&mut input)?
            }
        };
    }
    Ok(())
}

/// Loads, answers, then serves decodes until the pipe closes or a request needs the model gone:
/// an unload is answered after the natives are freed, and another load is handed back to run
/// after them.
fn loaded(
    input: &mut impl io::Read,
    output: &mut impl io::Write,
    id: u64,
    natives: &Path,
    models: &Path,
    root: impl FnOnce() -> Result<PathBuf, NativeError>,
) -> Result<Next, FrameError> {
    let sherpa = match c_path(models)
        .and_then(|_| root())
        .and_then(|root| Sherpa::load(natives, &root))
    {
        Ok(sherpa) => sherpa,
        Err(error) => {
            reply(output, id, &SpeechReply::Failed(error))?;
            return read(input);
        }
    };
    let recognizer = match sherpa.recognizer(models) {
        Ok(recognizer) => recognizer,
        Err(error) => {
            reply(output, id, &SpeechReply::Failed(error))?;
            return read(input);
        }
    };
    if let Err(error) = recognizer.decode(PREWARM_RATE, &PREWARM) {
        reply(output, id, &SpeechReply::Failed(error))?;
        return read(input);
    }
    reply(output, id, &SpeechReply::Loaded)?;
    loop {
        let Some((id, request)) = read(input)? else {
            return Ok(None);
        };
        match request {
            SpeechRequest::Decode { rate, pcm } => {
                reply(output, id, &decode(&recognizer, rate, &pcm))?;
            }
            SpeechRequest::Unload => {
                drop(recognizer);
                drop(sherpa);
                reply(output, id, &SpeechReply::Unloaded)?;
                return read(input);
            }
            #[cfg(debug_assertions)]
            SpeechRequest::Fault(fault) => inject(fault, output, id)?,
            load @ SpeechRequest::Load { .. } => {
                drop(recognizer);
                drop(sherpa);
                return Ok(Some((id, load)));
            }
        }
    }
}

/// The audio is checked again here: the worker trusts no request.
fn decode(recognizer: &Recognizer<'_>, rate: u32, pcm: &[u8]) -> SpeechReply {
    let (pairs, odd) = pcm.as_chunks::<2>();
    if !odd.is_empty() || check_audio(rate, pairs.len()).is_err() {
        return SpeechReply::Failed(NativeError::Input);
    }
    let samples: Vec<i16> = pairs.iter().map(|pair| i16::from_le_bytes(*pair)).collect();
    match recognizer.decode(rate, &samples) {
        Ok(text) => SpeechReply::Text(text),
        Err(error) => SpeechReply::Failed(error),
    }
}

fn read(input: &mut impl io::Read) -> Result<Next, FrameError> {
    read_request(input, Speech::MAX_REQUEST)
}

fn reply(output: &mut impl io::Write, id: u64, reply: &SpeechReply) -> Result<(), FrameError> {
    write_message(output, id, reply, Speech::MAX_REPLY)
}

#[cfg(debug_assertions)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "explicit dev-only panic-hook fixture"
)]
fn inject(
    fault: super::SpeechFault,
    output: &mut impl io::Write,
    id: u64,
) -> Result<(), FrameError> {
    use super::SpeechFault;
    match fault {
        SpeechFault::Hang => loop {
            std::thread::park();
        },
        SpeechFault::Crash => std::process::exit(1),
        SpeechFault::Abort => std::process::abort(),
        SpeechFault::AccessViolation => crate::platform::raise_access_violation(),
        SpeechFault::Garbage => {
            output.write_all(&[0xff; 4])?;
            output.flush()?;
            Ok(())
        }
        SpeechFault::Panic => panic!("private speech fault payload"),
        SpeechFault::Throw(path) => {
            let missed = match crate::platform::call_throwing_export(&path) {
                Err(_) => NativeError::Load,
                Ok(()) => NativeError::Decode,
            };
            reply(output, id, &SpeechReply::Failed(missed))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::Path;

    use super::{NativeError, Speech, SpeechReply, SpeechRequest, loaded};
    use crate::supervise::{Kind as _, read_request, write_message};

    fn load_refusal(models: &Path) -> SpeechReply {
        let mut queued = Vec::new();
        write_message(&mut queued, 42, &SpeechRequest::Unload, Speech::MAX_REQUEST)
            .expect("queued unload");
        let mut input = queued.as_slice();
        let mut output = Vec::new();
        let next = loaded(
            &mut input,
            &mut output,
            41,
            Path::new("\0"),
            models,
            || Err(NativeError::Unsafe),
        )
        .expect("load refusal and continuation");
        assert!(
            matches!(next, Some((42, SpeechRequest::Unload))),
            "the next request survives the failed load"
        );
        assert!(input.is_empty(), "the queued request was consumed once");
        let mut output = output.as_slice();
        let (id, reply) = read_request::<SpeechReply>(&mut output, Speech::MAX_REPLY)
            .expect("framed refusal")
            .expect("one reply");
        assert_eq!(id, 41, "the refusal answers the load");
        assert!(output.is_empty(), "exactly one reply");
        reply
    }

    #[test]
    fn an_unrepresentable_model_path_is_refused_without_native_storage() {
        let non_utf8 = OsString::from_wide(&[0xd800]);
        for models in [
            Path::new("\0"),
            Path::new("models\0private"),
            Path::new(&non_utf8),
        ] {
            assert_eq!(load_refusal(models), SpeechReply::Failed(NativeError::Input));
        }
    }

    #[test]
    fn representable_model_paths_keep_the_root_refusal() {
        for models in ["", ".", "models", "modèles", "C:\\models\\音声"] {
            assert_eq!(
                load_refusal(Path::new(models)),
                SpeechReply::Failed(NativeError::Unsafe)
            );
        }
    }
}
