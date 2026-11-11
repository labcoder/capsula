use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread::{Scope, ScopedJoinHandle};

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
    fail_stop(serve_requests)
}

fn fail_stop<T>(work: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(AssertUnwindSafe(work)) {
        Ok(served) => served,
        Err(_) => std::process::abort(),
    }
}

fn serve_requests() -> Result<(), FrameError> {
    let mut input = io::stdin().lock();
    let output = Mutex::new(io::stdout());
    let mut next = read(&mut input)?;
    while let Some((id, request)) = next {
        next = match request {
            SpeechRequest::Load { natives, models } => {
                loaded(&mut input, &output, id, &natives, &models, staging_root)?
            }
            SpeechRequest::Decode { .. } => {
                send(&output, id, &SpeechReply::NotLoaded)?;
                read(&mut input)?
            }
            SpeechRequest::Unload => {
                send(&output, id, &SpeechReply::Unloaded)?;
                read(&mut input)?
            }
            #[cfg(debug_assertions)]
            SpeechRequest::Fault(fault) => {
                inject(fault, &mut *output.lock().expect("worker output"), id)?;
                read(&mut input)?
            }
        };
    }
    Ok(())
}

/// Loads, answers, then serves decodes until the pipe closes or a request needs the model gone:
/// an unload is answered after the natives are freed, and another load is handed back to run
/// after them.
fn loaded<W: io::Write + Send>(
    input: &mut impl io::Read,
    output: &Mutex<W>,
    id: u64,
    natives: &Path,
    models: &Path,
    root: impl FnOnce() -> Result<PathBuf, NativeError>,
) -> Result<Next, FrameError> {
    loaded_with(input, output, id, models, || {
        c_path(models)
            .and_then(|_| root())
            .and_then(|root| Sherpa::load(natives, &root))
    })
}

fn loaded_with<W: io::Write + Send>(
    input: &mut impl io::Read,
    output: &Mutex<W>,
    id: u64,
    models: &Path,
    load: impl FnOnce() -> Result<Sherpa, NativeError>,
) -> Result<Next, FrameError> {
    let sherpa = match load() {
        Ok(sherpa) => sherpa,
        Err(error) => {
            send(output, id, &SpeechReply::Failed(error))?;
            return read(input);
        }
    };
    let created = sherpa.recognizer(models);
    if created.is_err() {
        let error = created.map(drop).expect_err("recognizer creation failed");
        drop(sherpa);
        send(output, id, &SpeechReply::Failed(error))?;
        return read(input);
    }
    let recognizer = created.expect("recognizer creation succeeded");
    if let Err(error) = recognizer.decode(PREWARM_RATE, &PREWARM) {
        drop(recognizer);
        drop(sherpa);
        send(output, id, &SpeechReply::Failed(error))?;
        return read(input);
    }
    send(output, id, &SpeechReply::Loaded)?;
    let next = std::thread::scope(|scope| {
        let mut slots = Slots::new(scope);
        loop {
            match read(input) {
                Ok(Some((id, SpeechRequest::Decode { rate, pcm }))) => {
                    let recognizer = &recognizer;
                    if slots
                        .start(move || send(output, id, &decode(recognizer, rate, &pcm)))
                        .is_err()
                    {
                        send(output, id, &SpeechReply::Failed(NativeError::Decode))?;
                    }
                }
                #[cfg(debug_assertions)]
                Ok(Some((id, SpeechRequest::Fault(fault)))) => {
                    if slots
                        .start(move || {
                            inject(fault, &mut *output.lock().expect("worker output"), id)
                        })
                        .is_err()
                    {
                        send(output, id, &SpeechReply::Failed(NativeError::Decode))?;
                    }
                }
                next => {
                    slots.drain();
                    return next;
                }
            }
        }
    });
    drop(recognizer);
    drop(sherpa);
    match next? {
        Some((id, SpeechRequest::Unload)) => {
            send(output, id, &SpeechReply::Unloaded)?;
            read(input)
        }
        next => Ok(next),
    }
}

struct Slots<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    running: [Option<ScopedJoinHandle<'scope, ()>>; 2],
    free: mpsc::SyncSender<usize>,
    freed: mpsc::Receiver<usize>,
}

impl<'scope, 'env> Slots<'scope, 'env> {
    fn new(scope: &'scope Scope<'scope, 'env>) -> Self {
        let (free, freed) = mpsc::sync_channel(2);
        Self {
            scope,
            running: [None, None],
            free,
            freed,
        }
    }

    fn start(
        &mut self,
        work: impl FnOnce() -> Result<(), FrameError> + Send + 'scope,
    ) -> io::Result<()> {
        let slot = self
            .running
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| self.reap());
        let free = self.free.clone();
        self.running[slot] = Some(
            std::thread::Builder::new()
                .name("speech-decode".into())
                .spawn_scoped(self.scope, move || {
                    fail_stop(|| {
                        if work().is_err() || free.send(slot).is_err() {
                            std::process::abort();
                        }
                    });
                })?,
        );
        Ok(())
    }

    fn reap(&mut self) -> usize {
        let slot = self
            .freed
            .recv()
            .expect("live decode slots publish completion");
        if self.running[slot]
            .take()
            .expect("the completed slot is occupied")
            .join()
            .is_err()
        {
            std::process::abort();
        }
        slot
    }

    fn drain(&mut self) {
        while self.running.iter().any(Option::is_some) {
            self.reap();
        }
    }
}

fn send(output: &Mutex<impl io::Write>, id: u64, value: &SpeechReply) -> Result<(), FrameError> {
    reply(&mut *output.lock().expect("worker output"), id, value)
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;

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
