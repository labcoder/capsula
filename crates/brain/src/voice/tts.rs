//! Spoken replies: one owner thread, `voice-tts`, turns text into PCM with Phonon at 24 kHz,
//! frame by frame, resamples it to the output's rate, and stops promptly when the generation
//! moves.
//!
//! Loads and unloads are asked for with an ask, and the thread reports each ask's outcome
//! (`LoadReport`) the same way speech input does: a load that finishes after its unload or a
//! newer ask is freed here and reported `Discarded`, and `unload` only touches its own ask.

mod phonon;
#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::watch;

use super::resample::Resampler;
use super::{LoadEvent, Reports};
use kumochi_protocol::message::MAX_SEGMENT_SECONDS;

pub(crate) const SAMPLE_RATE: u32 = 24_000;
pub(crate) use kumochi_protocol::message::MAX_SAY_BYTES;
/// +5.5 dB [S5]; the owner's feel session tunes it.
const GAIN: f32 = 1.883_649;

/// The files Phonon loads, under the spoken-replies models folder.
const CONFIG: &str = "config.json";
const WEIGHTS: &str = "model.q8.gguf";
const TOKENIZER: &str = "tokenizer.json";
const VOICE: &str = "juniper.safetensors";

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtsError {
    #[error("spoken replies aren't loaded")]
    NotLoaded,
    #[error("the text can't be spoken")]
    Text,
    #[error("spoken replies failed to load")]
    Load,
    #[error("speech synthesis failed")]
    Synthesis,
    #[error("speech synthesis produced an invalid sample")]
    NonFinite,
    #[error("the speech is longer than 30 seconds")]
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TtsBusy;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum TtsEvent {
    /// Mono at the Speak's rate: one Phonon frame, resampled.
    Pcm {
        segment: u32,
        samples: Vec<i16>,
    },
    /// The authoritative length in samples at the Speak's rate (M7).
    Done {
        segment: u32,
        total: u32,
    },
    Failed {
        segment: u32,
        error: TtsError,
    },
}

/// One segment to speak. `out` belongs to this Speak alone: PR 7 drops its receiver on
/// cancel, which is what fences old-generation events from the next Speak.
pub(crate) struct Speak {
    pub segment: u32,
    pub generation: u32,
    /// The output rate the audio is resampled to; a checked rate.
    pub rate: u32,
    pub text: String,
    pub out: mpsc::Sender<TtsEvent>,
}

impl Speak {
    /// Never waits: a full or closed channel just drops the report, and the receiver sees the
    /// channel close when this Speak is dropped.
    fn fail(self, error: TtsError) {
        self.out
            .try_send(TtsEvent::Failed {
                segment: self.segment,
                error,
            })
            .ok();
    }
}

/// What the synthesis loop needs from a model, so CI can run it without Phonon.
pub(crate) trait Engine {
    type Stream: Iterator<Item = Result<Vec<f32>, TtsError>>;
    fn stream(&self, text: &str) -> Result<Self::Stream, TtsError>;
}

struct State {
    /// The models folder wanted, with the ask that wants it.
    want: Option<(PathBuf, u64)>,
    /// The ask the owner is loading or holding a model for.
    current: Option<u64>,
    waiting: Option<Speak>,
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

    /// Changes the state and wakes the owner. The lock is never held across model work.
    fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let result = change(&mut self.lock());
        self.wake.notify_one();
        result
    }
}

pub(crate) struct TtsHandle {
    control: Arc<Control>,
    generation: watch::Sender<u32>,
    reports: Reports,
}

impl TtsHandle {
    pub(crate) fn spawn(runtime: Handle, reports: Reports) -> io::Result<Self> {
        Self::spawn_with(runtime, reports, phonon::Phonon::load)
    }

    pub(crate) fn spawn_with<E: Engine>(
        runtime: Handle,
        reports: Reports,
        load: impl FnMut(&Path) -> Result<E, TtsError> + Send + 'static,
    ) -> io::Result<Self> {
        let control = Arc::new(Control {
            state: Mutex::new(State {
                want: None,
                current: None,
                waiting: None,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let (generation, watcher) = watch::channel(0);
        let owner = Owner {
            control: Arc::clone(&control),
            generation: watcher,
            runtime,
            reports: reports.clone(),
        };
        std::thread::Builder::new()
            .name("voice-tts".into())
            .spawn(move || owner.run(load))?;
        Ok(Self {
            control,
            generation,
            reports,
        })
    }

    pub(crate) fn generation(&self) -> u32 {
        *self.generation.borrow()
    }

    /// Moves the generation: the owner drops its stream before the next frame or send.
    pub(crate) fn cancel(&self) -> u32 {
        self.generation.send_modify(|g| *g = g.wrapping_add(1));
        self.generation()
    }

    /// Loads `dir` for `ask`, replacing any earlier ask once the owner gets to it.
    pub(crate) fn load(&self, dir: PathBuf, ask: u64) {
        self.control.update(|state| state.want = Some((dir, ask)));
    }

    /// Unloads `ask` only, never a newer ask; an ask the owner doesn't have is reported
    /// `Unloaded` at once. Never waits on a channel: for its own ask it rejects the waiting
    /// Speak, cancels and wakes the owner. Taking the Speak before the cancel keeps the owner
    /// from dropping it as stale unanswered.
    pub(crate) fn unload(&self, ask: u64) {
        let mine = self.control.update(|state| {
            // Sent under the lock, so the owner's own report for `ask` can't come before it.
            if state.current != Some(ask) {
                self.reports.send(ask, LoadEvent::Unloaded);
            }
            if !wants(state, ask) {
                return None;
            }
            state.want = None;
            Some(state.waiting.take())
        });
        let Some(waiting) = mine else {
            return;
        };
        self.cancel();
        if let Some(speak) = waiting {
            speak.fail(TtsError::NotLoaded);
        }
    }

    /// A waiting Speak of an older generation is replaced unanswered, so a cancel never leaves
    /// the next Speak busy while the owner is loading.
    pub(crate) fn try_speak(&self, speak: Speak) -> Result<(), TtsBusy> {
        self.control.update(|state| {
            if state
                .waiting
                .as_ref()
                .is_some_and(|waiting| waiting.generation == speak.generation)
            {
                return Err(TtsBusy);
            }
            state.waiting = Some(speak);
            Ok(())
        })
    }
}

impl Drop for TtsHandle {
    /// The owner drops the stream and the model, then exits; this doesn't wait for it.
    fn drop(&mut self) {
        self.cancel();
        self.control.update(|state| state.shutdown = true);
    }
}

fn wants(state: &State, ask: u64) -> bool {
    state
        .want
        .as_ref()
        .is_some_and(|(_, wanted)| *wanted == ask)
}

enum Next {
    Exit,
    Unload,
    Load(PathBuf, u64),
    Speak(Speak),
}

struct Owner {
    control: Arc<Control>,
    generation: watch::Receiver<u32>,
    runtime: Handle,
    reports: Reports,
}

impl Owner {
    fn run<E: Engine>(mut self, mut load: impl FnMut(&Path) -> Result<E, TtsError>) {
        let mut engine: Option<(u64, E)> = None;
        loop {
            match self.next(engine.as_ref().map(|(ask, _)| *ask)) {
                Next::Exit => return,
                Next::Unload => {
                    self.free(&mut engine, None);
                }
                Next::Load(dir, ask) => {
                    if !self.free(&mut engine, Some(ask)) {
                        continue;
                    }
                    let loaded = load(&dir);
                    if let Err(error) = &loaded {
                        tracing::warn!(?error, "spoken replies failed to load");
                    }
                    // Every command sent during the load is in the state now; only the newest
                    // ask keeps its model.
                    let kept = loaded.is_ok()
                        && self.control.update(|state| {
                            let wanted = wants(state, ask);
                            if wanted {
                                self.reports.send(ask, LoadEvent::Loaded);
                            }
                            wanted
                        });
                    match loaded {
                        Ok(loaded) if kept => engine = Some((ask, loaded)),
                        rejected => {
                            let event = match rejected {
                                Ok(_) => LoadEvent::Discarded,
                                Err(_) => LoadEvent::Failed,
                            };
                            // The model drops while its ask is still current, so nothing
                            // reports it quiet before it's gone.
                            drop(rejected);
                            self.control.update(|state| {
                                state.current = None;
                                if event == LoadEvent::Failed && wants(state, ask) {
                                    state.want = None;
                                }
                                self.reports.send(ask, event);
                            });
                        }
                    }
                }
                Next::Speak(speak) => match &engine {
                    Some((_, loaded)) => self.speak(loaded, speak),
                    None => speak.fail(TtsError::NotLoaded),
                },
            }
        }
    }

    /// Drops the model in this thread, then reports its ask unloaded. `next` becomes current
    /// only if it's still wanted, since an unload during the drop already answered it absent;
    /// returns whether it did.
    fn free<E>(&self, engine: &mut Option<(u64, E)>, next: Option<u64>) -> bool {
        let Some((ask, loaded)) = engine.take() else {
            return true;
        };
        drop(loaded);
        self.control.update(|state| {
            state.current = next.filter(|next| wants(state, *next));
            self.reports.send(ask, LoadEvent::Unloaded);
            next.is_none() || state.current.is_some()
        })
    }

    fn next(&self, loaded: Option<u64>) -> Next {
        let mut state = self.control.lock();
        loop {
            if state.shutdown {
                return Next::Exit;
            }
            match (state.want.clone(), loaded) {
                (None, Some(_)) => return Next::Unload,
                (Some((dir, ask)), loaded) if loaded != Some(ask) => {
                    // A held model's ask stays current until it's freed.
                    if loaded.is_none() {
                        state.current = Some(ask);
                    }
                    return Next::Load(dir, ask);
                }
                _ => {}
            }
            if let Some(speak) = state.waiting.take() {
                return Next::Speak(speak);
            }
            state = self
                .control
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn moved(&self, mine: u32) -> bool {
        *self.generation.borrow() != mine
    }

    /// Returning drops the stream, which stops and joins Phonon's workers on this thread.
    fn speak<E: Engine>(&mut self, engine: &E, speak: Speak) {
        let Speak {
            segment,
            generation: mine,
            rate,
            text,
            out,
        } = speak;
        if self.moved(mine) {
            return;
        }
        if text.is_empty() || text.len() > MAX_SAY_BYTES || text.contains('\0') {
            self.deliver(
                &out,
                mine,
                TtsEvent::Failed {
                    segment,
                    error: TtsError::Text,
                },
            );
            return;
        }
        let mut stream = match engine.stream(&text) {
            Ok(stream) => stream,
            Err(error) => {
                self.deliver(&out, mine, TtsEvent::Failed { segment, error });
                return;
            }
        };
        let most = usize::try_from(rate * MAX_SEGMENT_SECONDS).unwrap_or(usize::MAX);
        let mut resampler = Resampler::new(SAMPLE_RATE, rate);
        let (mut total, mut audio) = (0usize, Vec::new());
        loop {
            if self.moved(mine) || out.is_closed() {
                return;
            }
            let frame = stream.next();
            if self.moved(mine) {
                return;
            }
            audio.clear();
            let ended = match frame {
                None => {
                    resampler.finish(&mut audio);
                    true
                }
                Some(Ok(frame)) if frame.iter().all(|sample| sample.is_finite()) => {
                    resampler.push(&frame, &mut audio);
                    false
                }
                Some(Ok(_)) => {
                    self.deliver(&out, mine, failed(segment, TtsError::NonFinite));
                    return;
                }
                Some(Err(error)) => {
                    self.deliver(&out, mine, failed(segment, error));
                    return;
                }
            };
            total += audio.len();
            let pcm = to_pcm(&audio);
            let failure = match &pcm {
                _ if total > most => Some(TtsError::TooLong),
                None => Some(TtsError::NonFinite),
                Some(_) => None,
            };
            if let Some(error) = failure {
                self.deliver(&out, mine, failed(segment, error));
                return;
            }
            if let Some(samples) = pcm.filter(|samples| !samples.is_empty())
                && !self.deliver(&out, mine, TtsEvent::Pcm { segment, samples })
            {
                return;
            }
            if ended {
                break;
            }
        }
        if !self.moved(mine) {
            let total = u32::try_from(total).unwrap_or(u32::MAX);
            self.deliver(&out, mine, TtsEvent::Done { segment, total });
        }
    }

    /// Sends without ever blocking on a full channel: it waits for room or for the generation to
    /// move, whichever comes first, and checks the generation again once room arrives.
    fn deliver(&mut self, out: &mpsc::Sender<TtsEvent>, mine: u32, event: TtsEvent) -> bool {
        let event = match out.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(event)) => event,
        };
        let generation = &mut self.generation;
        let permit = self.runtime.block_on(async {
            tokio::select! {
                biased;
                _ = generation.wait_for(|g| *g != mine) => None,
                permit = out.reserve() => permit.ok(),
            }
        });
        match permit {
            Some(permit) if !self.moved(mine) => {
                permit.send(event);
                true
            }
            _ => false,
        }
    }
}

fn failed(segment: u32, error: TtsError) -> TtsEvent {
    TtsEvent::Failed { segment, error }
}

impl fmt::Debug for TtsEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pcm { segment, samples } => f
                .debug_struct("Pcm")
                .field("segment", segment)
                .field("samples", &samples.len())
                .finish(),
            Self::Done { segment, total } => f
                .debug_struct("Done")
                .field("segment", segment)
                .field("total", total)
                .finish(),
            Self::Failed { segment, error } => f
                .debug_struct("Failed")
                .field("segment", segment)
                .field("error", error)
                .finish(),
        }
    }
}

/// Applies the gain, clips and converts; `None` if any sample is NaN or infinite.
fn to_pcm(frame: &[f32]) -> Option<Vec<i16>> {
    frame
        .iter()
        .map(|&sample| {
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                reason = "clamped to ±i16::MAX and rounded first"
            )]
            let pcm = ((sample * GAIN).clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            sample.is_finite().then_some(pcm)
        })
        .collect()
}
