//! Speech input's owner: one `voice-stt` thread that loads Parakeet, decodes and unloads, so the
//! coordinator never waits on native work.
//!
//! The handle and the thread share one small state behind a mutex. Load, unload and shutdown set
//! fields there, so none of them can be refused or held up by a decode in progress.

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

use crate::platform::sherpa::{NativeError, Recognizer, Sherpa};

/// The warm-up decode at load: one second of silence at 16 kHz [S5].
const PREWARM_RATE: u32 = 16_000;
static PREWARM: [i16; 16_000] = [0; 16_000];

/// One utterance to transcribe: mono PCM at its capture rate.
#[derive(Debug)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SttResult {
    Text(String),
    Failed(NativeError),
    /// Unload took the decode before it ran.
    Stale,
    /// Nothing is loaded yet, a load is in progress, or the last load failed.
    NotLoaded,
}

/// Another decode is already waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("speech input is busy")]
pub(crate) struct SttBusy;

/// What a loaded model can do; the real one is Parakeet's recognizer.
pub(crate) trait Decoder {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, NativeError>;
}

impl Decoder for Recognizer<'_> {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, NativeError> {
        Recognizer::decode(self, rate, samples)
    }
}

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
    /// Starts the worker, which loads the pinned natives from `natives` when asked to load.
    pub(crate) fn spawn(natives: PathBuf) -> io::Result<Self> {
        Self::spawn_with(move |models, serve| {
            let sherpa = Sherpa::load(&natives)?;
            let recognizer = sherpa.recognizer(models)?;
            recognizer.decode(PREWARM_RATE, &PREWARM)?;
            serve(&recognizer);
            Ok(())
        })
    }

    /// `engine` loads the models, hands the loaded decoder to `serve` until it returns, then
    /// tears everything down before it returns.
    fn spawn_with(
        engine: impl FnMut(&Path, &mut dyn FnMut(&dyn Decoder)) -> Result<(), NativeError>
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
            .spawn(move || run(&worker, engine))?;
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
    /// The worker finishes its native call, tears down and exits; this doesn't wait for it.
    fn drop(&mut self) {
        self.control.update(|state| state.shutdown = true);
    }
}

impl std::fmt::Debug for SttHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttHandle").finish_non_exhaustive()
    }
}

fn run(
    control: &Control,
    mut engine: impl FnMut(&Path, &mut dyn FnMut(&dyn Decoder)) -> Result<(), NativeError>,
) {
    while let Some(models) = next_load(control) {
        let mut serve = |decoder: &dyn Decoder| {
            while let Some(decode) = next_decode(control, &models) {
                if decode.reply.is_closed() {
                    continue;
                }
                let result = match decoder.decode(decode.rate, &decode.samples) {
                    Ok(text) => SttResult::Text(text),
                    Err(error) => SttResult::Failed(error),
                };
                decode.answer(result);
            }
        };
        if let Err(error) = engine(&models, &mut serve) {
            tracing::warn!(%error, "speech input failed to load");
            control.update(|state| {
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
