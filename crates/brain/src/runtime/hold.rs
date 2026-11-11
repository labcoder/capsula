//! One bounded pre-run voice owner; device debt survives connection loss.

mod audio;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, HOLD_DRAIN_TIMEOUT_MS, HOLD_START_TIMEOUT_MS, HoldFailure,
    HoldStatus, MAX_HOLD_CHUNKS, MicPcm, PartState, SettingsOp, ThreadId, ThreadOp,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use zeroize::Zeroize as _;

use super::{Brain, Effect, Full, Input, Link, Origin};
use crate::coordinator::{HoldKey, HoldSnapshot};
use crate::voice::stt::{Decode, SttHandle, SttResult};

#[derive(Debug, Default)]
struct Gate {
    active: AtomicU32,
    overflow: AtomicU32,
    wake: Notify,
}

pub(super) struct Packet(MicPcm);

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        self.0.samples.zeroize();
    }
}

pub(super) struct Ingress {
    send: mpsc::Sender<Packet>,
    gate: Arc<Gate>,
}

impl Ingress {
    pub(super) fn push(&self, chunk: MicPcm) {
        let packet = Packet(chunk);
        let gesture = packet.0.gesture.0;
        if gesture == 0 || self.gate.active.load(Ordering::SeqCst) != gesture {
            return;
        }
        if self.send.try_send(packet).is_err() {
            self.gate.overflow.store(gesture, Ordering::SeqCst);
            self.gate.wake.notify_one();
        }
    }
}

pub(super) struct Holds {
    high: u32,
    pending: Option<Hold>,
    debt: Option<HoldKey>,
    gate: Arc<Gate>,
    send: mpsc::Sender<Packet>,
    receive: mpsc::Receiver<Packet>,
}

struct Hold {
    key: HoldKey,
    thread: Option<ThreadId>,
    cancel: Arc<AtomicBool>,
    snapshot: Option<HoldSnapshot>,
    audio: Option<audio::Audio>,
    ask: Option<u64>,
    released: bool,
    deadline: Instant,
    decode: Option<oneshot::Receiver<SttResult>>,
    terminal: Option<HoldStatus>,
}

impl std::fmt::Debug for Holds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Holds")
            .field("pending", &self.pending)
            .field("debt", &self.debt)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hold")
            .field("key", &self.key)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl Default for Holds {
    fn default() -> Self {
        let (send, receive) = mpsc::channel(8);
        Self {
            high: 0,
            pending: None,
            debt: None,
            gate: Arc::default(),
            send,
            receive,
        }
    }
}

impl Holds {
    pub(super) fn ingress(&self) -> Ingress {
        Ingress {
            send: self.send.clone(),
            gate: Arc::clone(&self.gate),
        }
    }

    pub(super) fn invalidate(&mut self, status: HoldStatus) {
        if let Some(hold) = &mut self.pending {
            hold.cancel.store(true, Ordering::SeqCst);
            hold.terminal.get_or_insert(status);
        }
        self.gate.active.store(0, Ordering::SeqCst);
    }

    pub(super) fn lost(&mut self) {
        self.invalidate(HoldStatus::Cancelled);
        drop(self.take());
    }

    fn take(&mut self) -> Option<Hold> {
        self.gate.active.store(0, Ordering::SeqCst);
        while self.receive.try_recv().is_ok() {}
        self.pending.take()
    }

    pub(super) fn opened(&mut self) {
        self.high = 0;
    }

    pub(super) fn stopped(&mut self, key: HoldKey) {
        if self.debt == Some(key) {
            self.debt = None;
        }
    }
}

impl Drop for Holds {
    fn drop(&mut self) {
        self.lost();
    }
}

pub(super) enum Event {
    Packet(Packet),
    Overflow(u32),
    Decoded(HoldKey, SttResult),
    Deadline,
}

pub(super) async fn next(holds: &mut Holds) -> Event {
    let deadline = holds.pending.as_ref().map(|hold| hold.deadline);
    let key = holds.pending.as_ref().map(|hold| hold.key);
    let decode = holds.pending.as_mut().and_then(|hold| hold.decode.as_mut());
    tokio::select! {
        biased;
        () = super::sleep_until(deadline) => Event::Deadline,
        () = holds.gate.wake.notified() => Event::Overflow(holds.gate.overflow.swap(0, Ordering::SeqCst)),
        Some(packet) = holds.receive.recv() => Event::Packet(packet),
        result = async {
            match decode {
                Some(decode) => decode.await.unwrap_or(SttResult::Failed(crate::speech::SpeechError::WorkerLost)),
                None => std::future::pending().await,
            }
        } => Event::Decoded(key.expect("a decode owns its hold key"), result),
    }
}

impl Brain {
    fn hold_status(key: HoldKey, status: HoldStatus, link: &mut Link<'_, '_>) -> Result<(), Full> {
        link.control(&ControlToShell::VoiceHoldStatus {
            gesture: key.gesture,
            status,
        })
    }

    pub(super) fn hold_end(
        &mut self,
        status: HoldStatus,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        self.holds.invalidate(status);
        let Some(hold) = self.holds.take() else {
            return Ok(());
        };
        Self::hold_status(hold.key, hold.terminal.unwrap_or(status), link)
    }

    pub(super) fn hold_fence(&mut self, input: &Input) {
        let Some(hold) = &self.holds.pending else {
            return;
        };
        let status = match input {
            Input::Turn { .. } | Input::Ask { .. } | Input::ShellLost => {
                Some(HoldStatus::Cancelled)
            }
            Input::Delete { thread, .. } | Input::ThreadOp(ThreadOp::SetModel { thread, .. })
                if hold.thread == Some(*thread) =>
            {
                Some(HoldStatus::Failed(HoldFailure::ChoiceChanged))
            }
            Input::Engines(now) if self.coordinator.hold_availability_changes(now) => {
                Some(HoldStatus::Failed(HoldFailure::ChoiceChanged))
            }
            _ => None,
        };
        if let Some(status) = status {
            self.holds.invalidate(status);
        }
    }

    #[expect(clippy::too_many_lines, reason = "one bounded hold control router")]
    pub(super) fn hold_control(
        &mut self,
        message: &ControlToBrain,
        link: &mut Link<'_, '_>,
    ) -> Result<bool, Full> {
        let epoch = self.epoch;
        let key = |gesture| HoldKey { epoch, gesture };
        match message {
            ControlToBrain::VoiceHoldRecognized {
                gesture,
                thread,
                selection,
            } => {
                let key = key(*gesture);
                if gesture.0 <= self.holds.high {
                    return Ok(true);
                }
                self.holds.high = gesture.0;
                if self.holds.debt.is_some_and(|debt| debt.epoch != self.epoch) {
                    Self::hold_status(key, HoldStatus::Failed(HoldFailure::Unavailable), link)?;
                    return Ok(true);
                }
                if self.holds.pending.is_some()
                    || self.holds.debt.is_some()
                    || self.changing_model()
                {
                    Self::hold_status(key, HoldStatus::Failed(HoldFailure::Busy), link)?;
                    return Ok(true);
                }
                let (state, _) = self.hold_voice();
                if !matches!(state, PartState::Ready | PartState::Loading) {
                    let failure = if state == PartState::Unavailable {
                        HoldFailure::Unavailable
                    } else {
                        HoldFailure::NotLoaded
                    };
                    Self::hold_status(key, HoldStatus::Failed(failure), link)?;
                    return Ok(true);
                }
                let cancel = Arc::new(AtomicBool::new(false));
                let prepared = self.coordinator.prepare_hold(
                    key,
                    *thread,
                    *selection,
                    self.snapshot(Origin::Kumo),
                    Arc::clone(&cancel),
                );
                let Ok(op) = prepared else {
                    Self::hold_status(key, HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
                    return Ok(true);
                };
                self.holds.pending = Some(Hold {
                    key,
                    thread: *thread,
                    cancel,
                    snapshot: None,
                    audio: None,
                    ask: None,
                    released: false,
                    deadline: Instant::now() + crate::recall::control::READ_TIME,
                    decode: None,
                    terminal: None,
                });
                if state == PartState::Loading {
                    Self::hold_status(key, HoldStatus::Loading, link)?;
                }
                self.apply(vec![Effect::Store(op)], Some(link))
                    .map_err(|_| Full)?;
            }
            ControlToBrain::GestureCancel { gesture } => {
                self.holds.high = self.holds.high.max(gesture.0);
                let key = key(*gesture);
                if self
                    .holds
                    .pending
                    .as_ref()
                    .is_some_and(|hold| hold.key == key)
                {
                    self.hold_end(HoldStatus::Cancelled, link)?;
                }
                let effects = self.coordinator.cancel_hold(key);
                self.apply(effects, Some(link)).map_err(|_| Full)?;
            }
            ControlToBrain::KeyUp { gesture, .. }
            | ControlToBrain::VoiceHoldStopped { gesture } => {
                self.holds.high = self.holds.high.max(gesture.0);
                let key = key(*gesture);
                if matches!(message, ControlToBrain::VoiceHoldStopped { .. }) {
                    self.holds.stopped(key);
                }
                let Some(hold) = self.holds.pending.as_mut().filter(|hold| hold.key == key) else {
                    return Ok(true);
                };
                let Some(audio) = &mut hold.audio else {
                    let status = if hold.ask.is_some() {
                        HoldStatus::Failed(HoldFailure::InvalidAudio)
                    } else {
                        HoldStatus::Cancelled
                    };
                    self.hold_end(status, link)?;
                    return Ok(true);
                };
                let result = if let ControlToBrain::KeyUp { chunks, .. } = message {
                    hold.released = true;
                    audio.end(*chunks)
                } else {
                    audio.stopped()
                };
                if let Err(failure) = result {
                    self.hold_end(HoldStatus::Failed(failure), link)?;
                } else {
                    if hold.released && self.holds.debt.is_none() {
                        hold.deadline = Instant::now()
                            + Duration::from_millis(u64::from(HOLD_DRAIN_TIMEOUT_MS));
                    }
                    self.hold_decode(link)?;
                }
            }
            ControlToBrain::Settings(SettingsOp::SetDefaultModel(_))
            | ControlToBrain::AccountOp(_)
            | ControlToBrain::Connection(_) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
                let effects = self.coordinator.cancel_current_hold();
                self.apply(effects, Some(link)).map_err(|_| Full)?;
                return Ok(false);
            }
            ControlToBrain::Settings(SettingsOp::RemoveVoice(
                kumochi_protocol::voice::VoicePart::SpeechInput,
            )) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::NotLoaded), link)?;
                return Ok(false);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn hold_prepared(
        &mut self,
        key: HoldKey,
        result: Result<HoldSnapshot, ()>,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        let Some(hold) = self.holds.pending.as_mut().filter(|hold| hold.key == key) else {
            return Ok(());
        };
        if Instant::now() >= hold.deadline {
            return self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link);
        }
        match result {
            Ok(snapshot) if !hold.cancel.load(Ordering::SeqCst) => hold.snapshot = Some(snapshot),
            _ => return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link),
        }
        hold.deadline = Instant::now() + crate::speech::DEADLINES.load;
        self.hold_voice_changed(link)
    }

    pub(super) fn hold_voice_changed(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        let Some(hold) = &self.holds.pending else {
            return Ok(());
        };
        if hold.cancel.load(Ordering::SeqCst) {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        if Instant::now() >= hold.deadline {
            return self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link);
        }
        if hold
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !self.coordinator.admits_hold(snapshot))
        {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        let (state, stt) = self.hold_voice();
        if state == PartState::Loading && hold.ask.is_none() {
            return Ok(());
        }
        if state != PartState::Ready {
            let failure = if state == PartState::Unavailable {
                HoldFailure::Unavailable
            } else {
                HoldFailure::NotLoaded
            };
            return self.hold_end(HoldStatus::Failed(failure), link);
        }
        if let Some(ask) = hold.ask {
            if stt.is_none_or(|stt| !stt.current_use(ask)) {
                return self.hold_end(HoldStatus::Failed(HoldFailure::NotLoaded), link);
            }
            return Ok(());
        }
        if hold.snapshot.is_none() {
            return Ok(());
        }
        if !self.admits_turn(link) {
            return self.hold_end(HoldStatus::Failed(HoldFailure::Busy), link);
        }
        let admitted = stt
            .ok_or(HoldFailure::NotLoaded)
            .and_then(SttHandle::admit_use);
        let ask = match admitted {
            Ok(ask) => ask,
            Err(failure) => return self.hold_end(HoldStatus::Failed(failure), link),
        };
        let hold = self.holds.pending.as_mut().expect("pending admission");
        hold.ask = Some(ask);
        hold.audio = Some(audio::Audio::new());
        hold.deadline = Instant::now()
            + Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS + MAX_HOLD_CHUNKS * 20));
        self.holds.debt = Some(hold.key);
        self.holds.gate.overflow.store(0, Ordering::SeqCst);
        self.holds
            .gate
            .active
            .store(hold.key.gesture.0, Ordering::SeqCst);
        Self::hold_status(hold.key, HoldStatus::Admitted, link)
    }

    fn hold_decode(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        let Some(hold) = &mut self.holds.pending else {
            return Ok(());
        };
        if self.holds.gate.overflow.load(Ordering::SeqCst) == hold.key.gesture.0 {
            return self.hold_end(HoldStatus::Failed(HoldFailure::InvalidAudio), link);
        }
        if !self.holds.receive.is_empty() {
            return Ok(());
        }
        if hold.decode.is_some() {
            return Ok(());
        }
        let Some(samples) = hold.audio.as_mut().and_then(audio::Audio::take) else {
            return Ok(());
        };
        hold.audio = None;
        if samples.is_empty() {
            return self.hold_end(HoldStatus::Empty, link);
        }
        let audio = Duration::from_millis(
            u64::try_from(samples.len()).expect("bounded PCM") * 1000 / u64::from(audio::RATE),
        );
        hold.deadline = Instant::now()
            + crate::speech::DEADLINES.decode
            + audio * crate::speech::DEADLINES.decode_per_mille / 1000;
        let (reply, receive) = oneshot::channel();
        let ask = hold.ask.expect("admitted audio");
        let key = hold.key;
        let decode = Decode {
            rate: audio::RATE,
            samples,
            reply,
        };
        let result = if let Some(stt) = self.hold_voice().1 {
            stt.decode_for(ask, decode)
        } else {
            decode.discard();
            Err(HoldFailure::NotLoaded)
        };
        if let Err(failure) = result {
            return self.hold_end(HoldStatus::Failed(failure), link);
        }
        self.holds.pending.as_mut().expect("pending decode").decode = Some(receive);
        Self::hold_status(key, HoldStatus::Transcribing, link)
    }

    pub(super) fn hold_event(&mut self, event: Event, link: &mut Link<'_, '_>) -> Result<(), Full> {
        if let Event::Decoded(key, _) = &event
            && self
                .holds
                .pending
                .as_ref()
                .is_none_or(|hold| hold.key != *key)
        {
            return Ok(());
        }
        if self
            .holds
            .pending
            .as_ref()
            .is_some_and(|hold| Instant::now() >= hold.deadline)
        {
            return self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link);
        }
        if self
            .holds
            .pending
            .as_ref()
            .is_some_and(|hold| hold.cancel.load(Ordering::SeqCst))
        {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        match event {
            Event::Deadline => self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link),
            Event::Overflow(gesture) => {
                if self
                    .holds
                    .pending
                    .as_ref()
                    .is_some_and(|hold| hold.key.gesture.0 == gesture)
                {
                    self.hold_end(HoldStatus::Failed(HoldFailure::InvalidAudio), link)?;
                }
                Ok(())
            }
            Event::Packet(packet) => {
                let Some(hold) = self
                    .holds
                    .pending
                    .as_mut()
                    .filter(|hold| hold.key.gesture == packet.0.gesture)
                else {
                    return Ok(());
                };
                let Some(audio) = &mut hold.audio else {
                    return self.hold_end(HoldStatus::Failed(HoldFailure::InvalidAudio), link);
                };
                if let Err(failure) = audio.push(&packet.0) {
                    return self.hold_end(HoldStatus::Failed(failure), link);
                }
                self.hold_decode(link)
            }
            Event::Decoded(_, SttResult::Text(text)) => {
                if text.trim().is_empty() {
                    return self.hold_end(HoldStatus::Empty, link);
                }
                // shortcut: busy transfer discards the hold; retain its bounded cancellable owner until admission opens when queued voice is needed.
                if !self.admits_turn(link) {
                    return self.hold_end(HoldStatus::Failed(HoldFailure::Busy), link);
                }
                let Some(hold) = self.holds.take() else {
                    return Ok(());
                };
                let snapshot = hold.snapshot.expect("decode has a pinned snapshot");
                match self.coordinator.submit_hold(snapshot, text.into()) {
                    Ok(effects) => self.apply(effects, Some(link)).map_err(|_| Full),
                    Err(()) => Self::hold_status(
                        hold.key,
                        HoldStatus::Failed(HoldFailure::ChoiceChanged),
                        link,
                    ),
                }
            }
            Event::Decoded(_, SttResult::NotLoaded | SttResult::Stale) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::NotLoaded), link)
            }
            Event::Decoded(_, SttResult::Failed(_)) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::DecodeFailed), link)
            }
        }
    }
}
