//! Speech input's owner: one `voice-stt` thread that has the speech worker load Parakeet, decode
//! and unload, so the coordinator never waits on that work. sherpa-onnx runs only in the worker
//! process (see `crate::speech`).
//!
//! The handle and the thread share one small state behind a mutex. Load, unload and shutdown set
//! fields there, so none of them can be refused or held up by a decode in progress. Every load is
//! asked for with an ask, and the thread reports what became of each one (`LoadReport`): a load
//! that finishes after a newer ask or its own unload is freed in the thread, never reported
//! `Loaded`, and `unload` only ever touches its own ask.

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::oneshot;

use super::{LoadEvent, Reports};
use crate::speech::{SpeechError, SpeechHandle};
use crate::supervise::Generation;

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

/// The models a load serves, with the worker generation that loaded them, or the loss that ended
/// it.
type Engine = dyn FnMut(&Path, &mut dyn FnMut(Generation, &dyn Decoder) -> Served) -> Result<(), SpeechError>
    + Send;

struct State {
    /// The models wanted, with the ask that wants them.
    want: Option<(PathBuf, u64)>,
    /// The ask the thread is loading or serving.
    current: Option<u64>,
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
    reports: Reports,
}

impl SttHandle {
    /// Starts the owner thread, which has `speech` stage the natives from `natives` and load the
    /// models when asked to load. `runtime` carries the worker's requests.
    pub(crate) fn spawn(
        runtime: Handle,
        speech: SpeechHandle,
        natives: PathBuf,
        reports: Reports,
    ) -> io::Result<Self> {
        let losses = speech.lost();
        let wakes = speech.lost();
        let tasks = runtime.clone();
        let handle = Self::spawn_with(reports, losses, move |models, serve| {
            let generation = runtime.block_on(speech.load(natives.clone(), models.to_owned()))?;
            let remote = Remote {
                speech: &speech,
                runtime: &runtime,
            };
            if serve(generation, &remote) == Served::Lost {
                return Err(SpeechError::WorkerLost);
            }
            runtime.block_on(speech.unload())
        })?;
        tasks.spawn(wake_on_losses(Arc::clone(&handle.control), wakes));
        Ok(handle)
    }

    /// `engine` loads the models, hands the loaded decoder and the generation that loaded it to
    /// `serve` until it returns, then unloads before it returns. A loss reported by `serve` is
    /// returned as an error. `losses` is the thread's own subscription to worker losses.
    fn spawn_with(
        reports: Reports,
        losses: broadcast::Receiver<Generation>,
        engine: impl FnMut(
            &Path,
            &mut dyn FnMut(Generation, &dyn Decoder) -> Served,
        ) -> Result<(), SpeechError>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let control = Arc::new(Control {
            state: Mutex::new(State {
                want: None,
                current: None,
                ready: false,
                waiting: None,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&control);
        let thread_reports = reports.clone();
        std::thread::Builder::new()
            .name("voice-stt".into())
            .spawn(move || run(&worker, &thread_reports, losses, Box::new(engine)))?;
        Ok(Self { control, reports })
    }

    /// Loads `models` for `ask`, replacing any earlier ask: a model served for another ask is
    /// torn down after its running decode, and the new one is loaded.
    pub(crate) fn load(&self, models: PathBuf, ask: u64) {
        self.control
            .update(|state| state.want = Some((models, ask)));
    }

    /// Unloads `ask` only, never a newer ask. Never waits: a waiting decode gets `Stale`, and a
    /// decode in progress finishes first. The thread reports `Unloaded` once the model is
    /// freed; an ask the thread doesn't have is reported `Unloaded` at once.
    pub(crate) fn unload(&self, ask: u64) {
        let waiting = self.control.update(|state| {
            // Sent under the lock, so the thread's own report for `ask` can't come before it.
            if state.current != Some(ask) {
                self.reports.send(ask, LoadEvent::Unloaded);
            }
            if !wants(state, ask) {
                return None;
            }
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

/// Wakes the thread on each loss, so it reads the loss from its own subscription and an idle loss
/// ends serving too.
async fn wake_on_losses(control: Arc<Control>, mut losses: broadcast::Receiver<Generation>) {
    loop {
        match losses.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => control.update(|_| ()),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Whether the subscription holds the loss of `served`, the worker holding the model. An older
/// worker's loss can arrive after a newer load, since the supervisor answers before it broadcasts,
/// so it's skipped. A lag drops only the oldest losses, and a later one still counts.
fn lost(losses: &mut broadcast::Receiver<Generation>, served: Generation) -> bool {
    loop {
        match losses.try_recv() {
            Ok(dead) if dead < served => {}
            Ok(_) => return true,
            Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return false,
        }
    }
}

/// Loads, serves and unloads until shutdown, reporting each ask's outcome once its model is
/// freed. A failed load, or a lost worker, leaves speech input unloaded unless a newer ask came
/// after that load started: the running decode was answered `Failed(WorkerLost)`, and a waiting
/// one is answered `NotLoaded`.
fn run(
    control: &Control,
    reports: &Reports,
    mut losses: broadcast::Receiver<Generation>,
    mut engine: Box<Engine>,
) {
    while let Some((models, ask)) = next_load(control) {
        let mut ended = None;
        let mut serve = |generation: Generation, decoder: &dyn Decoder| {
            // Every command sent during the load is in the state now; only the newest ask is kept.
            let kept = control.update(|state| {
                let kept = wants(state, ask);
                if kept {
                    reports.send(ask, LoadEvent::Loaded);
                }
                kept
            });
            if !kept {
                ended = Some(LoadEvent::Discarded);
                return Served::Released;
            }
            let served = serve_decodes(control, ask, generation, decoder, &mut losses);
            ended = Some(match served {
                Served::Lost => LoadEvent::Lost,
                Served::Released => LoadEvent::Unloaded,
            });
            served
        };
        let result = engine(&models, &mut serve);
        let event = match (result, ended) {
            (Ok(()), Some(event)) => event,
            (Err(SpeechError::WorkerLost), _) | (_, Some(LoadEvent::Lost)) => LoadEvent::Lost,
            (Err(error), None) => {
                tracing::warn!(%error, "speech input failed to load");
                LoadEvent::Failed
            }
            (Err(error), Some(_)) => {
                tracing::warn!(%error, "speech input failed to unload");
                LoadEvent::Lost
            }
            (Ok(()), None) => LoadEvent::Failed,
        };
        let waiting = control.update(|state| {
            state.ready = false;
            state.current = None;
            reports.send(ask, event);
            if !matches!(event, LoadEvent::Failed | LoadEvent::Lost) {
                return None;
            }
            if wants(state, ask) {
                state.want = None;
            }
            state.waiting.take()
        });
        if let Some(decode) = waiting {
            decode.answer(SttResult::NotLoaded);
        }
    }
}

fn wants(state: &State, ask: u64) -> bool {
    state
        .want
        .as_ref()
        .is_some_and(|(_, wanted)| *wanted == ask)
}

/// Serves decodes while `ask` stays wanted and the worker `generation` isn't lost.
fn serve_decodes(
    control: &Control,
    ask: u64,
    generation: Generation,
    decoder: &dyn Decoder,
    losses: &mut broadcast::Receiver<Generation>,
) -> Served {
    loop {
        let decode = match next_decode(control, ask, generation, losses) {
            Ok(Some(decode)) => decode,
            Ok(None) => return Served::Released,
            Err(Lost) => return Served::Lost,
        };
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
}

/// The models to load next with the ask they answer, or `None` on shutdown. A decode left
/// still waiting once nothing is wanted is answered `NotLoaded`.
fn next_load(control: &Control) -> Option<(PathBuf, u64)> {
    let mut state = control.lock();
    loop {
        if state.shutdown {
            return None;
        }
        if let Some((models, ask)) = state.want.clone() {
            state.current = Some(ask);
            return Some((models, ask));
        }
        if let Some(decode) = state.waiting.take() {
            decode.answer(SttResult::NotLoaded);
        }
        state = control.wait(state);
    }
}

/// The worker was lost while serving.
struct Lost;

/// The next decode while `ask` stays wanted, `None` once it isn't or on shutdown, or `Lost`.
fn next_decode(
    control: &Control,
    ask: u64,
    generation: Generation,
    losses: &mut broadcast::Receiver<Generation>,
) -> Result<Option<Decode>, Lost> {
    let mut state = control.lock();
    loop {
        if lost(losses, generation) {
            state.ready = false;
            return Err(Lost);
        }
        let wanted = !state.shutdown && wants(&state, ask);
        state.ready = wanted;
        if !wanted {
            return Ok(None);
        }
        if let Some(decode) = state.waiting.take() {
            return Ok(Some(decode));
        }
        state = control.wait(state);
    }
}
