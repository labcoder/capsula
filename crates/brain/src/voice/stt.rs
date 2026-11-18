//! Speech input's owner: one `voice-stt` thread that loads, schedules at most two decodes and
//! unloads through the speech worker, so the coordinator never waits. sherpa-onnx runs only in the worker
//! process (see `crate::speech`).
//!
//! The handle and the thread share one small state behind a mutex. Load, unload and shutdown set
//! fields there, so none of them can be refused or held up by a decode in progress. Every load is
//! asked for with an ask, and the thread reports what became of each one (`LoadReport`): a load
//! that finishes after a newer ask or its own unload is freed in the thread, never reported
//! `Loaded`, and `unload` only ever touches its own ask.

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::held_for_runtime;
#[cfg(test)]
pub(crate) use tests::held_for_runtime_checked;
#[cfg(test)]
pub(crate) use tests::hold_before_serving;
#[cfg(test)]
pub(crate) use tests::{pending_results, runtime_decoder, wanted_load};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ScopedJoinHandle;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{HoldFailure, SttUnload};
use tokio::runtime::Handle;
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::{oneshot, watch};
use zeroize::{Zeroize as _, Zeroizing};

use super::{LoadEvent, Reports};
use crate::speech::{SpeechError, SpeechHandle};
use crate::supervise::{Dispatch, Exit, Generation, Retirement};

#[cfg(test)]
mod poll_cut;
#[cfg(test)]
mod receipt_tests;
#[cfg(test)]
pub(crate) use poll_cut::Cut;

async fn released(
    dispatch: &mut watch::Receiver<Dispatch>,
    exits: &mut broadcast::Receiver<Exit>,
    retirement: &Retirement,
) -> Result<(), SpeechError> {
    loop {
        let state = *dispatch.borrow_and_update();
        match state {
            Dispatch::Pending => tokio::select! {
                biased;
                changed = dispatch.changed() => changed.map_err(|_| SpeechError::ExitUnconfirmed)?,
                () = retirement.closed() => return Err(SpeechError::ExitUnconfirmed),
            },
            Dispatch::NotDispatched => return Ok(()),
            Dispatch::Dispatched(generation) => {
                return confirmed(exits, generation, retirement).await;
            }
        }
    }
}

async fn confirmed(
    exits: &mut broadcast::Receiver<Exit>,
    generation: Generation,
    retirement: &Retirement,
) -> Result<(), SpeechError> {
    let mut closed = retirement.request(generation).is_err();
    loop {
        let exit = if closed {
            exits.try_recv().map_err(|_| SpeechError::ExitUnconfirmed)?
        } else {
            tokio::select! {
                biased;
                exit = exits.recv() => exit.map_err(|_| SpeechError::ExitUnconfirmed)?,
                () = retirement.closed() => {
                    closed = true;
                    continue;
                }
            }
        };
        if exit.generation == generation {
            return if exit.confirmed {
                Ok(())
            } else {
                Err(SpeechError::ExitUnconfirmed)
            };
        }
    }
}

/// One utterance to transcribe: mono PCM at its capture rate.
pub(crate) struct Decode {
    pub rate: u32,
    pub samples: Vec<i16>,
    pub reply: oneshot::Sender<SttResult>,
}

impl Decode {
    /// A receiver that went away doesn't need the answer.
    fn answer(mut self, result: SttResult) {
        self.samples.zeroize();
        self.reply.send(result).ok();
    }

    pub(crate) fn discard(mut self) {
        self.samples.zeroize();
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

impl SttResult {
    fn is_service_loss(&self) -> bool {
        matches!(
            self,
            Self::Failed(
                SpeechError::WorkerLost | SpeechError::ExitUnconfirmed | SpeechError::NotLoaded
            )
        )
    }
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
pub(crate) trait Decoder: Sync {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError>;
}

/// How serving a loaded model ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Served {
    /// The model stopped being wanted, or the handle dropped.
    Released,
    /// The worker can no longer serve; its resources still need a release receipt.
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
    replies: [Option<oneshot::Sender<SttResult>>; 2],
    finished: [Option<SttResult>; 2],
    #[cfg(test)]
    decode_cut: Option<DecodeCut>,
    /// Holds the worker between the model becoming ready and serving.
    #[cfg(test)]
    hold_after_ready: bool,
    unload_after: Option<Duration>,
    idle_since: Instant,
    shutdown: bool,
}

impl State {
    fn has_live_decode(&self) -> bool {
        self.replies
            .iter()
            .flatten()
            .any(|reply| !reply.is_closed())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeCut {
    Spawn,
    Delivery,
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
    pub(crate) fn admit_use(&self) -> Result<u64, HoldFailure> {
        self.control.update(|state| {
            let ask = state.current.ok_or(HoldFailure::NotLoaded)?;
            if state.shutdown || !state.ready || !wants(state, ask) {
                return Err(HoldFailure::NotLoaded);
            }
            if state.waiting.is_some() || state.has_live_decode() {
                return Err(HoldFailure::Busy);
            }
            state.idle_since = Instant::now();
            Ok(ask)
        })
    }

    pub(crate) fn current_use(&self, ask: u64) -> bool {
        let state = self.control.lock();
        !state.shutdown && state.ready && state.current == Some(ask) && wants(&state, ask)
    }

    pub(crate) fn decode_for(&self, ask: u64, mut decode: Decode) -> Result<(), HoldFailure> {
        self.control.update(|state| {
            let failure = if state.shutdown
                || !state.ready
                || state.current != Some(ask)
                || !wants(state, ask)
            {
                Some(HoldFailure::NotLoaded)
            } else if state.waiting.is_some() || state.has_live_decode() {
                Some(HoldFailure::Busy)
            } else {
                None
            };
            if let Some(failure) = failure {
                decode.samples.zeroize();
                return Err(failure);
            }
            state.waiting = Some(decode);
            Ok(())
        })
    }

    /// Starts the owner thread, which has `speech` stage the natives from `natives` and load the
    /// models when asked to load. `runtime` carries the worker's requests.
    pub(crate) fn spawn(
        runtime: Handle,
        speech: SpeechHandle,
        natives: PathBuf,
        reports: Reports,
    ) -> io::Result<Self> {
        Self::spawn_adapter(
            runtime,
            speech,
            natives,
            reports,
            #[cfg(test)]
            None,
        )
    }

    fn spawn_adapter(
        runtime: Handle,
        speech: SpeechHandle,
        natives: PathBuf,
        reports: Reports,
        #[cfg(test)] mut cut: Option<poll_cut::Gate>,
    ) -> io::Result<Self> {
        let losses = speech.lost();
        let wakes = speech.lost();
        let tasks = runtime.clone();
        let handle = Self::spawn_with(reports, losses, move |models, serve| {
            #[cfg(test)]
            let _clock = cut.as_ref().map(|_| runtime.enter());
            let mut exits = speech.exits();
            let retirement = speech.retirement();
            let (dispatch, mut receipt) = watch::channel(Dispatch::Pending);
            let generation = match runtime.block_on({
                let request = speech.load_tracked(natives.clone(), models.to_owned(), dispatch);
                #[cfg(test)]
                let request = poll_cut::hold(&mut cut, Cut::Load, request);
                request
            }) {
                Ok(generation) => generation,
                Err(error @ SpeechError::Native(_)) => return Err(error),
                Err(_) => {
                    runtime.block_on({
                        let request = released(&mut receipt, &mut exits, &retirement);
                        #[cfg(test)]
                        let request = poll_cut::observe_wait(&mut cut, request);
                        request
                    })?;
                    return Err(SpeechError::WorkerLost);
                }
            };
            let remote = Remote {
                speech: &speech,
                runtime: &runtime,
            };
            if serve(generation, &remote) == Served::Released
                && runtime.block_on({
                    let request = speech.unload_owned(generation);
                    #[cfg(test)]
                    let request = poll_cut::hold(&mut cut, Cut::Unload, request);
                    request
                }) == Ok(generation)
            {
                return Ok(());
            }
            runtime.block_on({
                let request = confirmed(&mut exits, generation, &retirement);
                #[cfg(test)]
                let request = poll_cut::observe_wait(&mut cut, request);
                request
            })?;
            Err(SpeechError::WorkerLost)
        })?;
        tasks.spawn(wake_on_losses(Arc::clone(&handle.control), wakes));
        Ok(handle)
    }

    /// `engine` loads the models, hands the loaded decoder and the generation that loaded it to
    /// `serve` until it returns, then unloads before it returns. A loss reported by `serve` is
    /// returned as an error after release is proved, or `ExitUnconfirmed` if it cannot be.
    /// `losses` is the thread's own subscription to worker losses.
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
                replies: [None, None],
                finished: [None, None],
                #[cfg(test)]
                decode_cut: None,
                #[cfg(test)]
                hold_after_ready: false,
                unload_after: None,
                idle_since: Instant::now(),
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
        self.control.update(|state| {
            if !state.shutdown {
                state.want = Some((models, ask));
            }
        });
    }

    /// Applies the configured idle policy without interrupting a load or decode.
    pub(crate) fn set_unload(&self, policy: SttUnload) {
        self.control.update(|state| {
            state.unload_after = match policy {
                SttUnload::Kept => None,
                SttUnload::After15Min => Some(Duration::from_mins(15)),
                SttUnload::After1Hour => Some(Duration::from_hours(1)),
            };
        });
    }

    /// Unloads `ask` only, never a newer ask. Never waits: a waiting decode gets `Stale`, and a
    /// decode in progress finishes first. The thread reports `Unloaded` once the model is
    /// freed; an ask the thread doesn't have is reported `Unloaded` at once.
    pub(crate) fn unload(&self, ask: u64) {
        let waiting = self.control.update(|state| {
            if state.shutdown {
                return None;
            }
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
                decode.discard();
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
        let waiting = self.control.update(|state| {
            state.shutdown = true;
            state.waiting.take()
        });
        if let Some(decode) = waiting {
            decode.answer(SttResult::Stale);
        }
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
                let kept = !state.shutdown && state.current == Some(ask) && wants(state, ask);
                state.ready = kept;
                if state.ready {
                    // The idle clock starts with readiness, so a change made once ready stays.
                    state.idle_since = Instant::now();
                    reports.send(ask, LoadEvent::Loaded);
                }
                kept
            });
            if !kept {
                ended = Some(LoadEvent::Discarded);
                return Served::Released;
            }
            #[cfg(test)]
            {
                let mut state = control.lock();
                while state.hold_after_ready {
                    state = control.wait(state);
                }
            }
            let served = serve_decodes(control, ask, generation, decoder, &mut losses);
            ended = Some(match served {
                Served::Lost => LoadEvent::Lost,
                Served::Released => LoadEvent::Unloaded,
            });
            served
        };
        let result = engine(&models, &mut serve);
        if result == Err(SpeechError::ExitUnconfirmed) {
            // A dropped handle means the brain is quitting, and its runtime ends the supervisor.
            if !control.update(|state| state.shutdown) {
                tracing::error!(
                    "speech worker exit unconfirmed, so speech input stays unavailable"
                );
            }
            let waiting = control.update(|state| {
                state.ready = false;
                state.shutdown = true;
                reports.send(ask, LoadEvent::Unavailable);
                state.waiting.take()
            });
            if let Some(decode) = waiting {
                decode.answer(SttResult::NotLoaded);
            }
            return;
        }
        let event = match (result, ended) {
            (Ok(()), Some(event)) => event,
            (Err(SpeechError::WorkerLost), _) | (_, Some(LoadEvent::Lost)) => LoadEvent::Lost,
            (Err(SpeechError::Native(crate::platform::sherpa::NativeError::ModelPath)), None) => {
                LoadEvent::ModelPath
            }
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
            if !matches!(
                event,
                LoadEvent::Failed | LoadEvent::Lost | LoadEvent::ModelPath
            ) {
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
    std::thread::scope(|scope| {
        let mut running = [None, None];
        let mut failed = false;
        let mut state = control.lock();
        loop {
            #[cfg(test)]
            while state.decode_cut == Some(DecodeCut::Delivery) {
                state = control.wait(state);
            }
            state = settle_decodes(
                control,
                state,
                &mut running,
                ask,
                generation,
                losses,
                &mut failed,
            );
            let occupied = running.iter().filter(|task| task.is_some()).count();
            if !state.ready {
                if occupied == 0 {
                    return if failed {
                        Served::Lost
                    } else {
                        Served::Released
                    };
                }
                state = control.wait(state);
                continue;
            }
            let live = state.has_live_decode();
            if occupied < 2
                && !live
                && let Some(decode) = state.waiting.take()
            {
                let slot = running
                    .iter()
                    .position(Option::is_none)
                    .expect("a free slot");
                #[cfg(test)]
                let cut = state.decode_cut.take();
                state.replies[slot] = Some(decode.reply);
                drop(state);
                let spawned = spawn_decode(
                    scope,
                    control,
                    decoder,
                    slot,
                    decode.rate,
                    decode.samples,
                    #[cfg(test)]
                    cut,
                );
                state = control.lock();
                if let Ok(task) = spawned {
                    running[slot] = Some(task);
                } else {
                    state.ready = false;
                    failed = true;
                    if let Some(reply) = state.replies[slot].take() {
                        reply.send(SttResult::Failed(SpeechError::WorkerLost)).ok();
                    }
                }
                continue;
            }
            let wait = if occupied == 0 {
                match state.unload_after {
                    None => None,
                    Some(after) => {
                        let Some(left) = after
                            .checked_sub(state.idle_since.elapsed())
                            .filter(|left| !left.is_zero())
                        else {
                            state.ready = false;
                            state.want = None;
                            return Served::Released;
                        };
                        Some(left)
                    }
                }
            } else if occupied == 1 && live && state.waiting.is_some() {
                Some(Duration::from_millis(10))
            } else {
                None
            };
            state = match wait {
                Some(wait) => {
                    control
                        .wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => control.wait(state),
            };
        }
    })
}

fn settle_decodes<'state>(
    control: &'state Control,
    mut state: MutexGuard<'state, State>,
    running: &mut [Option<ScopedJoinHandle<'_, ()>>; 2],
    ask: u64,
    generation: Generation,
    losses: &mut broadcast::Receiver<Generation>,
    failed: &mut bool,
) -> MutexGuard<'state, State> {
    *failed |= lost(losses, generation);
    let active = running.iter().any(Option::is_some);
    let mut completed = [None, None];
    while let Some(slot) = state.finished.iter().position(Option::is_some) {
        *failed |= state
            .finished
            .iter()
            .flatten()
            .any(SttResult::is_service_loss);
        if *failed {
            state.ready = false;
        }
        completed[slot] = state.finished[slot].take();
        let task = running[slot].take().expect("a completion owns its slot");
        drop(state);
        let joined = task.join();
        state = control.lock();
        *failed |= joined.is_err();
        *failed |= lost(losses, generation);
    }
    *failed |= lost(losses, generation);
    if active && running.iter().all(Option::is_none) {
        state.idle_since = Instant::now();
    }
    state.ready = !*failed && !state.shutdown && wants(&state, ask);
    for (slot, result) in completed.into_iter().enumerate() {
        if let Some(result) = result
            && let Some(reply) = state.replies[slot].take()
        {
            reply
                .send(if *failed {
                    SttResult::Failed(SpeechError::WorkerLost)
                } else {
                    result
                })
                .ok();
        }
    }
    if *failed {
        for reply in &mut state.replies {
            if let Some(reply) = reply.take() {
                reply.send(SttResult::Failed(SpeechError::WorkerLost)).ok();
            }
        }
        if let Some(decode) = state.waiting.take() {
            decode.answer(SttResult::NotLoaded);
        }
    }
    if state
        .waiting
        .as_ref()
        .is_some_and(|decode| decode.reply.is_closed())
        && let Some(decode) = state.waiting.take()
    {
        decode.answer(SttResult::Stale);
    }
    state
}

fn spawn_decode<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    control: &'env Control,
    decoder: &'env dyn Decoder,
    slot: usize,
    rate: u32,
    samples: Vec<i16>,
    #[cfg(test)] cut: Option<DecodeCut>,
) -> io::Result<ScopedJoinHandle<'scope, ()>> {
    let samples = Zeroizing::new(samples);
    #[cfg(test)]
    if cut == Some(DecodeCut::Spawn) {
        return Err(io::Error::other("decode thread refused"));
    }
    std::thread::Builder::new()
        .name("voice-decode".into())
        .spawn_scoped(scope, move || {
            #[cfg(panic = "unwind")]
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decoder.decode(rate, &samples)
            }))
            .unwrap_or(Err(SpeechError::WorkerLost));
            #[cfg(not(panic = "unwind"))]
            let result = decoder.decode(rate, &samples);
            let result = match result {
                Ok(text) => SttResult::Text(text),
                Err(error) => SttResult::Failed(error),
            };
            control.update(|state| {
                if result.is_service_loss() {
                    state.ready = false;
                }
                state.finished[slot] = Some(result);
            });
        })
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
