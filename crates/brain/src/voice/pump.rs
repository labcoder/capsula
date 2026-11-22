//! The brain's one reader of TTS. It holds the current Speak's receiver and forwards audio only
//! while the run's [`Reply`](super::reply::Reply) has granted credit, so forwarded audio never passes
//! the window, and a full channel parks the TTS thread instead of the core loop. A segment's end
//! needs no credit, so a full window never hides it.

#[cfg(test)]
mod tests;

use std::future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use kumochi_protocol::message::{RunId, SegmentId};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::tts::{Speak, TtsError, TtsEvent, TtsHandle};

/// TTS events one Speak's channel holds before the TTS thread parks [guess].
const CHANNEL: usize = 4;

// shortcut: five seconds is provisional until first PCM is measured on the current build.
const FIRST_PCM_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Legacy,
    Conducted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FirstPcmDeadline {
    run: RunId,
    segment: SegmentId,
    generation: u32,
    pub(crate) at: Instant,
}

struct Active {
    segment: SegmentId,
    generation: u32,
    events: mpsc::Receiver<TtsEvent>,
    first_pcm: Option<Instant>,
}

pub(crate) struct Pump {
    tts: TtsHandle,
    output: OutputMode,
    /// The run whose speech the pump serves; only a Speak changes it.
    run: Option<RunId>,
    /// Samples the pump may still read: granted minus read. A frame may overshoot it.
    credit: u64,
    active: Option<Active>,
    /// The one event read ahead: audio waiting for credit, which nothing passes.
    held: Option<TtsEvent>,
}

impl std::fmt::Debug for Pump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pump")
            .field("run", &self.run)
            .field("credit", &self.credit)
            .field(
                "segment",
                &self.active.as_ref().map(|active| active.segment),
            )
            .field("held", &self.held.is_some())
            .finish_non_exhaustive()
    }
}

impl Pump {
    pub(crate) fn new(tts: TtsHandle, output: OutputMode) -> Self {
        Self {
            tts,
            output,
            run: None,
            credit: 0,
            active: None,
            held: None,
        }
    }

    pub(crate) fn output(&self) -> OutputMode {
        self.output
    }

    /// The TTS handle the pump reads, for loads and unloads.
    pub(crate) fn tts(&self) -> &TtsHandle {
        &self.tts
    }

    /// Hands TTS one segment of `run` at the output `rate`, ending any Speak still held.
    pub(crate) fn speak(&mut self, run: RunId, segment: SegmentId, rate: u32, text: String) {
        self.serve(run);
        self.stop();
        let (out, events) = mpsc::channel(CHANNEL);
        let generation = self.tts.generation();
        let speak = Speak {
            segment: segment.0,
            generation,
            rate,
            text,
            out,
        };
        // A busy owner drops the Speak, which closes `events`: the segment fails like any
        // stream that ends early.
        let dispatched = self.tts.try_speak(speak).is_ok();
        if !dispatched {
            tracing::warn!("tts was busy; the segment fails");
        }
        self.active = Some(Active {
            segment,
            generation,
            events,
            first_pcm: (dispatched && self.output == OutputMode::Conducted)
                .then(|| Instant::now() + FIRST_PCM_LIMIT),
        });
    }

    /// Lets the pump forward `samples` more of `run`'s audio. Credit for a run the pump doesn't
    /// serve, such as a late one from a run already replaced, changes nothing.
    pub(crate) fn credit(&mut self, run: RunId, samples: u64) {
        if self.run == Some(run) {
            self.credit = self.credit.saturating_add(samples);
        }
    }

    /// Stops `run`'s current Speak; credit stays for its next one.
    pub(crate) fn cancel(&mut self, run: RunId) {
        if self.run == Some(run) {
            self.stop();
        }
    }

    /// Stops `run`'s speech for good.
    pub(crate) fn revoke(&mut self, run: RunId) {
        if self.run == Some(run) {
            self.stop();
            self.credit = 0;
        }
    }

    pub(crate) fn first_pcm_deadline(&self) -> Option<FirstPcmDeadline> {
        let active = self.active.as_ref()?;
        Some(FirstPcmDeadline {
            run: self.run?,
            segment: active.segment,
            generation: active.generation,
            at: active.first_pcm?,
        })
    }

    pub(crate) fn expire_first_pcm(&mut self, due: FirstPcmDeadline) -> Option<(RunId, SegmentId)> {
        if Instant::now() < due.at || self.first_pcm_deadline() != Some(due) {
            return None;
        }
        let active = self.active.as_mut()?;
        active.first_pcm = None;
        if self.held.is_some() {
            return None;
        }
        // try_recv can park behind a sender that claimed a slot but hasn't published it.
        let event = std::pin::pin!(tokio::task::unconstrained(active.events.recv()))
            .poll(&mut Context::from_waker(Waker::noop()));
        self.held = Some(match event {
            Poll::Ready(Some(event)) => event,
            Poll::Ready(None) => closed(due.segment),
            Poll::Pending => {
                self.active = None;
                if self.tts.generation() == due.generation {
                    self.tts.cancel();
                }
                return Some((due.run, due.segment));
            }
        });
        None
    }

    /// The next TTS event for the run served. Audio waits for credit; `Done`, a failure, or a
    /// stream that closes without `Done` (a synthesis failure) don't, though they never pass
    /// audio still waiting. Pending while there's nothing to forward; it's cancel-safe.
    pub(crate) async fn next(&mut self) -> (RunId, TtsEvent) {
        let (Some(run), Some(active)) = (self.run, self.active.as_mut()) else {
            return future::pending().await;
        };
        if self.held.is_none() {
            let event = active
                .events
                .recv()
                .await
                .unwrap_or_else(|| closed(active.segment));
            active.first_pcm = None;
            self.held = Some(event);
        }
        if self.credit == 0 && matches!(self.held, Some(TtsEvent::Pcm { .. })) {
            return future::pending().await;
        }
        let event = self.held.take().expect("an event was just held");
        match &event {
            TtsEvent::Pcm { samples, .. } => {
                let read = u64::try_from(samples.len()).unwrap_or(u64::MAX);
                self.credit = self.credit.saturating_sub(read);
            }
            TtsEvent::Done { .. } | TtsEvent::Failed { .. } => self.active = None,
        }
        (run, event)
    }

    fn serve(&mut self, run: RunId) {
        if self.run != Some(run) {
            self.stop();
            self.run = Some(run);
            self.credit = 0;
        }
    }

    /// Moves TTS's generation and drops the receiver and any held event, which fences every
    /// event still coming.
    fn stop(&mut self) {
        self.held = None;
        if self.active.take().is_some() {
            self.tts.cancel();
        }
    }
}

fn closed(segment: SegmentId) -> TtsEvent {
    TtsEvent::Failed {
        segment: segment.0,
        error: TtsError::Synthesis,
    }
}
