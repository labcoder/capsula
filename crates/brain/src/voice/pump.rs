//! The brain's one reader of TTS. It holds the current Speak's receiver and forwards audio only
//! while the run's [`Reply`](super::reply::Reply) has granted credit, so forwarded audio never passes
//! the window, and a full channel parks the TTS thread instead of the core loop. A segment's end
//! needs no credit, so a full window never hides it.

#[cfg(test)]
mod tests;

use std::future;

use kumochi_protocol::message::{RunId, SegmentId};
use tokio::sync::mpsc;

use super::tts::{Speak, TtsError, TtsEvent, TtsHandle};

/// TTS events one Speak's channel holds before the TTS thread parks [guess].
const CHANNEL: usize = 4;

pub(crate) struct Pump {
    tts: TtsHandle,
    /// The run whose speech the pump serves; only a Speak changes it.
    run: Option<RunId>,
    /// Samples the pump may still read: granted minus read. A frame may overshoot it.
    credit: u64,
    active: Option<(SegmentId, mpsc::Receiver<TtsEvent>)>,
    /// The one event read ahead: audio waiting for credit, which nothing passes.
    held: Option<TtsEvent>,
}

impl Pump {
    pub(crate) fn new(tts: TtsHandle) -> Self {
        Self {
            tts,
            run: None,
            credit: 0,
            active: None,
            held: None,
        }
    }

    /// Hands TTS one segment of `run` at the output `rate`, ending any Speak still held.
    pub(crate) fn speak(&mut self, run: RunId, segment: SegmentId, rate: u32, text: String) {
        self.serve(run);
        self.stop();
        let (out, events) = mpsc::channel(CHANNEL);
        let speak = Speak {
            segment: segment.0,
            generation: self.tts.generation(),
            rate,
            text,
            out,
        };
        // A busy owner drops the Speak, which closes `events`: the segment fails like any
        // stream that ends early.
        if self.tts.try_speak(speak).is_err() {
            tracing::warn!("tts was busy; the segment fails");
        }
        self.active = Some((segment, events));
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

    /// The next TTS event for the run served. Audio waits for credit; `Done`, a failure, or a
    /// stream that closes without `Done` (a synthesis failure) don't, though they never pass
    /// audio still waiting. Pending while there's nothing to forward; it's cancel-safe.
    pub(crate) async fn next(&mut self) -> (RunId, TtsEvent) {
        let (Some(run), Some((segment, events))) = (self.run, self.active.as_mut()) else {
            return future::pending().await;
        };
        if self.held.is_none() {
            let event = events.recv().await.unwrap_or(TtsEvent::Failed {
                segment: segment.0,
                error: TtsError::Synthesis,
            });
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
