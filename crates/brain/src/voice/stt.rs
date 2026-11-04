//! Speech input's owner: one `voice-stt` thread that has the speech worker load Parakeet, decode
//! and unload, so the coordinator never waits on that work. sherpa-onnx runs only in the worker
//! process (see `crate::speech`).
//!
//! The handle and the thread share one small state behind a mutex. Load, unload and shutdown set
//! fields there, so none of them can be refused or held up by a decode in progress.

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::speech::{SpeechError, SpeechHandle};

/// One utterance to transcribe: mono PCM at its capture rate.
pub(crate) struct Decode {
    pub rate: u32,
    pub samples: Vec<i16>,
    pub reply: oneshot::Sender<SttResult>,
}

impl Decode {
    /// A receiver that went away doesn't need the answer.
    fn answer(self, result: SttResult) {
        self.reply.send(result).ok();
    }
}

/// The rate and the sample count, never the samples.
impl std::fmt::Debug for Decode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decode")
            .field("rate", &self.rate)
            .field("samples", &self.samples.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SttResult {
    Text(String),
    /// The decode failed, or the worker was lost while it ran.
    Failed(SpeechError),
    /// Unload took the decode before it ran.
    Stale,
    /// Nothing is loaded yet, a load is in progress, the last load failed, or the worker was lost.
    NotLoaded,
}

/// The variant and the transcript's length, never its text.
impl std::fmt::Debug for SttResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Self::Failed(error) => write!(f, "Failed({error:?})"),
            Self::Stale => f.write_str("Stale"),
            Self::NotLoaded => f.write_str("NotLoaded"),
        }
    }
}

/// Another decode is already waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("speech input is busy")]
pub(crate) struct SttBusy;

/// What a loaded model can do; the real one asks the speech worker.
pub(crate) trait Decoder {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError>;
}

/// How serving a loaded model ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Served {
    /// The model stopped being wanted, or the handle dropped.
    Released,
    /// The worker, and the model with it, is gone.
    Lost,
}

/// The speech worker, asked from the `voice-stt` thread through the brain's runtime.
struct Remote<'a> {
    speech: &'a SpeechHandle,
    runtime: &'a Handle,
}

impl Decoder for Remote<'_> {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError> {
        self.runtime.block_on(self.speech.decode(rate, samples))
    }
}

/// The models a load serves, or the loss that ended it.
type Engine =
    dyn FnMut(&Path, &mut dyn FnMut(&dyn Decoder) -> Served) -> Result<(), SpeechError> + Send;

struct State {
    want: Option<PathBuf>,
    /// Set by the worker only while a loaded model serves `want`.
    ready: bool,
    waiting: Option<Decode>,
    shutdown: bool,
}

struct Control {
    state: Mutex<State>,
    wake: Condvar,
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and wakes the worker. The lock is never held across native work.
    fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let result = change(&mut self.lock());
        self.wake.notify_one();
        result
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.wake
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct SttHandle {
    control: Arc<Control>,
}

impl SttHandle {
    /// Starts the owner thread, which has `speech` stage the natives from `natives` and load the
    /// models when asked to load. `runtime` carries the worker's requests.
    pub(crate) fn spawn(
        runtime: Handle,
        speech: SpeechHandle,
        natives: PathBuf,
    ) -> io::Result<Self> {
        Self::spawn_with(move |models, serve| {
            runtime.block_on(speech.load(natives.clone(), models.to_owned()))?;
            let remote = Remote {
                speech: &speech,
                runtime: &runtime,
            };
            if serve(&remote) == Served::Lost {
                return Err(SpeechError::WorkerLost);
            }
            runtime.block_on(speech.unload())
        })
    }

    /// `engine` loads the models, hands the loaded decoder to `serve` until it returns, then
    /// unloads before it returns. A loss reported by `serve` is returned as an error.
    fn spawn_with(
        engine: impl FnMut(&Path, &mut dyn FnMut(&dyn Decoder) -> Served) -> Result<(), SpeechError>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let control = Arc::new(Control {
            state: Mutex::new(State {
                want: None,
                ready: false,
                waiting: None,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&control);
        std::thread::Builder::new()
            .name("voice-stt".into())
            .spawn(move || run(&worker, Box::new(engine)))?;
        Ok(Self { control })
    }

    pub(crate) fn load(&self, models: PathBuf) {
        self.control.update(|state| state.want = Some(models));
    }

    /// Never waits: a waiting decode gets `Stale`, and a decode in progress finishes first.
    pub(crate) fn unload(&self) {
        let waiting = self.control.update(|state| {
            state.want = None;
            state.waiting.take()
        });
        if let Some(decode) = waiting {
            decode.answer(SttResult::Stale);
        }
    }

    /// Queues `decode` behind the one in progress. While no model is ready, or once unload is
    /// asked for even with a decode still running, it's answered `NotLoaded` at once.
    pub(crate) fn try_decode(&self, decode: Decode) -> Result<(), SttBusy> {
        let refused = self.control.update(|state| {
            if !state.ready || state.want.is_none() {
                return Ok(Some(decode));
            }
            if state.waiting.is_some() {
                return Err(SttBusy);
            }
            state.waiting = Some(decode);
            Ok(None)
        })?;
        if let Some(decode) = refused {
            decode.answer(SttResult::NotLoaded);
        }
        Ok(())
    }
}

impl Drop for SttHandle {
    /// The thread finishes the request in flight, unloads and exits; this doesn't wait for it.
    fn drop(&mut self) {
        self.control.update(|state| state.shutdown = true);
    }
}

impl std::fmt::Debug for SttHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttHandle").finish_non_exhaustive()
    }
}

/// Loads, serves and unloads until shutdown. A failed load, or a lost worker, leaves speech
/// input unloaded until the next `load`: the running decode was answered `Failed(WorkerLost)`,
/// and a waiting one is answered `NotLoaded` by [`next_load`].
fn run(control: &Control, mut engine: Box<Engine>) {
    while let Some(models) = next_load(control) {
        let mut serve = |decoder: &dyn Decoder| {
            while let Some(decode) = next_decode(control, &models) {
                if decode.reply.is_closed() {
                    continue;
                }
                match decoder.decode(decode.rate, &decode.samples) {
                    Ok(text) => decode.answer(SttResult::Text(text)),
                    // A worker that answers `NotLoaded` here was replaced since the load.
                    Err(SpeechError::WorkerLost | SpeechError::NotLoaded) => {
                        decode.answer(SttResult::Failed(SpeechError::WorkerLost));
                        return Served::Lost;
                    }
                    Err(error) => decode.answer(SttResult::Failed(error)),
                }
            }
            Served::Released
        };
        if let Err(error) = engine(&models, &mut serve) {
            tracing::warn!(%error, "speech input stopped");
            control.update(|state| {
                state.ready = false;
                if state.want.as_ref() == Some(&models) {
                    state.want = None;
                }
            });
        }
    }
}

/// The models to load next, or `None` on shutdown. A decode left waiting by a failed load is
/// answered `NotLoaded`.
fn next_load(control: &Control) -> Option<PathBuf> {
    let mut state = control.lock();
    loop {
        if state.shutdown {
            return None;
        }
        if let Some(models) = &state.want {
            return Some(models.clone());
        }
        if let Some(decode) = state.waiting.take() {
            decode.answer(SttResult::NotLoaded);
        }
        state = control.wait(state);
    }
}

/// The next decode while `models` stays wanted, or `None` once it isn't or on shutdown.
fn next_decode(control: &Control, models: &Path) -> Option<Decode> {
    let mut state = control.lock();
    loop {
        let wanted = !state.shutdown && state.want.as_deref() == Some(models);
        state.ready = wanted;
        if !wanted {
            return None;
        }
        if let Some(decode) = state.waiting.take() {
            return Some(decode);
        }
        state = control.wait(state);
    }
}
