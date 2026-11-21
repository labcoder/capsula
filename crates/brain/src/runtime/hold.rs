//! One bounded pre-run voice owner; device debt survives connection loss.

mod audio;
#[cfg(test)]
mod reconnect_tests;
mod screen;
#[cfg(test)]
pub(super) mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use kumochi_protocol::message::{
    CaptureFailure, CaptureFor, ConnectionKind, ControlToBrain, ControlToShell, DataToShell,
    HOLD_DRAIN_TIMEOUT_MS, HOLD_START_TIMEOUT_MS, HoldFailure, HoldStatus, MAX_HOLD_CHUNKS,
    MAX_TRANSCRIPT_BYTES, MicPcm, ModelChoice, ModelSlug, NoticeRevision, PartState, ScreenClass,
    ScreenHolds, ScreenOff, SettingsOp, ThreadId, ThreadOp,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use zeroize::Zeroize as _;

use super::{Brain, Effect, Full, Input, Link, Origin};
use crate::coordinator::{HoldKey, HoldSnapshot};
use crate::platform::ShellProcess;
use crate::screen::HoldClient;
use crate::voice::stt::{Decode, SttHandle, SttResult};
use screen::{Outcome, ScreenEvent, ScreenPart, capturing};

#[derive(Debug)]
enum FailureStage {
    Status,
    Admission,
    Preparation,
    ReleaseDebt,
    VoiceAvailability,
    Decode,
}

#[derive(Debug)]
enum Refusal {
    StaleGesture,
    StaleCompletion,
    ReadOrCancellation,
}

/// Why a screen hold's part didn't start.
#[derive(Debug)]
enum NoScreen {
    Gate(ScreenOff),
    /// Recognized, or for a screen hold released, longer ago than `LATEST_START`.
    TooLate,
    /// No worker client, or the hold isn't pending or already has a part.
    NotStarted,
    /// A release whose path has more than `MAX_PATH_POINTS` points.
    PathTooLong,
    /// A release for no screen hold, for one that isn't armed, or for one whose choice changed.
    NotArmed,
    /// A release whose exclusion stamp is earlier than its own stamp.
    ExcludedEarly,
    /// A release whose exclusion stamp is later than now, or can't be read.
    ExcludedLate,
    /// A hold not under the offer last sent, or under one that no longer holds; a window hold.
    Unoffered,
}

impl NoScreen {
    /// What an armed part closes as when its release starts nothing.
    fn part(&self) -> ScreenPart {
        match self {
            Self::Gate(ScreenOff::TurnedOff) => ScreenPart::Discarded,
            Self::Gate(ScreenOff::ModelCantSee) => ScreenPart::Closed(ScreenClass::ModelCantSee),
            _ => ScreenPart::Closed(ScreenClass::NotReady),
        }
    }
}

/// Which hold a recognition admitted: a screen hold's part follows the show-screen setting, a
/// window hold's the screen-holds setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Voice,
    Window,
    Screen,
}

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
    debt_peer: Option<ShellProcess>,
    /// A stop receipt this connection sent while its shell process couldn't be checked; the
    /// connection's tick checks it again, idle or not.
    unacked: Option<HoldKey>,
    peer: Option<ShellProcess>,
    gate: Arc<Gate>,
    send: mpsc::Sender<Packet>,
    receive: mpsc::Receiver<Packet>,
    /// The screen worker's client; 2c4 gives the brain one.
    screen: Option<Arc<HoldClient>>,
    /// Whether this platform captures one window; a client alone doesn't say so.
    window_capture: bool,
    /// The gate's state last sent on this connection, unstamped, with the sign-in it spoke for.
    screen_sent: Option<(ScreenHolds, Option<crate::context::ConnectionTag>)>,
    /// The revision of the last offer sent on this connection.
    notice: Option<NoticeRevision>,
    /// How long ago a shell's input stamp was taken; tests replace the clock.
    stamp_age: fn(u64) -> Option<Duration>,
    /// Where each hold's screen task sends its answers.
    screen_events: mpsc::Sender<ScreenEvent>,
    screen_answers: mpsc::Receiver<ScreenEvent>,
}

struct Hold {
    key: HoldKey,
    kind: Kind,
    thread: Option<ThreadId>,
    cancel: Arc<AtomicBool>,
    snapshot: Option<HoldSnapshot>,
    audio: Option<audio::Audio>,
    ask: Option<u64>,
    released: bool,
    deadline: Instant,
    decode: Option<oneshot::Receiver<SttResult>>,
    terminal: Option<HoldStatus>,
    screen: ScreenPart,
    screen_recipient: Option<(ConnectionKind, ModelSlug)>,
    /// The offer revision an armed screen part was armed under; its release must carry it.
    armed_notice: Option<NoticeRevision>,
    /// The transcript, while it waits for a screen hold's picture.
    words: Option<String>,
}

impl Hold {
    fn matches_screen_recipient(&self, choice: Option<&ModelChoice>) -> bool {
        self.screen_recipient
            .as_ref()
            .zip(choice)
            .is_some_and(|((connection, model), choice)| {
                *connection == choice.connection && *model == choice.model
            })
    }
}

impl std::fmt::Debug for Holds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Holds")
            .field("pending", &self.pending)
            .field("debt", &self.debt)
            .finish_non_exhaustive()
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        debug_assert!(
            std::thread::panicking()
                || !(capturing(&self.screen) || matches!(self.screen, ScreenPart::Ready(_))),
            "a hold ended past both of its screen part's closers"
        );
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
        let (screen_events, screen_answers) = mpsc::channel(8);
        Self {
            high: 0,
            pending: None,
            debt: None,
            debt_peer: None,
            unacked: None,
            peer: None,
            gate: Arc::default(),
            send,
            receive,
            screen: None,
            window_capture: crate::platform::WINDOW_CAPTURE,
            screen_sent: None,
            notice: None,
            stamp_age: crate::platform::qpc_age,
            screen_events,
            screen_answers,
        }
    }
}

impl Holds {
    /// Whether this platform can capture and the screen worker started.
    pub(super) fn can_capture(&self) -> bool {
        self.window_capture && self.screen.is_some()
    }

    fn connected(&mut self, peer: ShellProcess) {
        self.retire_exited_peer();
        self.unacked = None;
        self.peer = Some(peer);
    }

    fn retire_exited_peer(&mut self) {
        if self
            .debt_peer
            .as_ref()
            .is_some_and(|peer| matches!(peer.exited(), Ok(true)))
        {
            self.debt = None;
            self.debt_peer = None;
        }
    }

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
        self.close_screen();
        self.gate.active.store(0, Ordering::SeqCst);
    }

    pub(super) fn lost(&mut self) {
        self.invalidate(HoldStatus::Cancelled);
        drop(self.take());
    }

    fn take(&mut self) -> Option<Hold> {
        self.close_screen();
        self.gate.active.store(0, Ordering::SeqCst);
        while self.receive.try_recv().is_ok() {}
        self.pending.take()
    }

    pub(super) fn opened(&mut self) {
        self.high = 0;
        self.screen_sent = None;
        self.notice = None;
    }

    /// None when the shell process could not be checked, so the receipt stays unacknowledged
    /// until its connection's tick checks it again.
    pub(super) fn stopped(&mut self, key: HoldKey) -> Option<bool> {
        let Some(debt) = self.debt.filter(|debt| debt.gesture == key.gesture) else {
            return Some(false);
        };
        if debt != key {
            let (Some(old), Some(current)) = (&self.debt_peer, &self.peer) else {
                return Some(false);
            };
            if !old.same_live_process(current).ok()? {
                return Some(false);
            }
        }
        self.debt = None;
        self.debt_peer = None;
        Some(true)
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
    Screen(ScreenEvent),
}

pub(super) async fn next(holds: &mut Holds) -> Event {
    let deadline = holds.pending.as_ref().map(|hold| hold.deadline);
    let key = holds.pending.as_ref().map(|hold| hold.key);
    let decode = holds.pending.as_mut().map(|hold| &mut hold.decode);
    tokio::select! {
        biased;
        () = super::sleep_until(deadline) => Event::Deadline,
        () = holds.gate.wake.notified() => Event::Overflow(holds.gate.overflow.swap(0, Ordering::SeqCst)),
        Some(packet) = holds.receive.recv() => Event::Packet(packet),
        Some(event) = holds.screen_answers.recv() => Event::Screen(event),
        result = async {
            let Some(slot) = decode else { return std::future::pending().await };
            let Some(receive) = slot.as_mut() else { return std::future::pending().await };
            let result = receive.await.unwrap_or(SttResult::Failed(crate::speech::SpeechError::WorkerLost));
            // An answered receiver panics if polled again, and a screen hold's words keep it pending.
            *slot = None;
            result
        } => Event::Decoded(key.expect("a decode owns its hold key"), result),
    }
}

impl Brain {
    pub(crate) fn connected_shell(&mut self, peer: ShellProcess) {
        self.holds.connected(peer);
    }

    /// Whether a stop receipt of this connection waits for its shell process to be checked again.
    pub(super) fn owes_stop(&self) -> bool {
        self.holds
            .unacked
            .is_some_and(|stop| stop.epoch == self.epoch)
    }

    /// Checks again a stop receipt whose shell process couldn't be checked when it came, and
    /// answers it once the check gives an answer, as its own arrival would have.
    pub(super) fn retry_stop(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        let Some(stop) = self.holds.unacked.filter(|stop| stop.epoch == self.epoch) else {
            return Ok(());
        };
        if self.holds.stopped(stop).is_none() {
            return Ok(());
        }
        self.holds.unacked = None;
        link.control(&ControlToShell::VoiceHoldStopAccepted {
            gesture: stop.gesture,
        })
    }

    /// Gives screen holds the client of the brain's one screen worker.
    pub(crate) fn attach_screen(&mut self, client: Arc<HoldClient>) {
        self.holds.screen = Some(client);
    }

    fn hold_status(key: HoldKey, status: HoldStatus, link: &mut Link<'_, '_>) -> Result<(), Full> {
        if let HoldStatus::Failed(reason) = status {
            tracing::warn!(stage = ?FailureStage::Status, ?reason, "held speech input failed");
        } else {
            tracing::debug!(stage = ?FailureStage::Status, reason = ?status, "held speech input boundary");
        }
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
            Input::Engines(kind, now)
                if self.coordinator.hold_availability_changes(
                    *kind,
                    now,
                    hold.snapshot.as_ref(),
                ) =>
            {
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
        // One admission for both; a screen hold then adds its part, or releases the lease.
        if let ControlToBrain::HoldRecognized {
            gesture,
            thread,
            selection,
            ..
        } = message
        {
            // No shell sends a window hold now, and its capture would start before the hold's
            // recipient is read: it goes on as voice, and its lease ends.
            if self.admit_hold(key(*gesture), *thread, *selection, Kind::Window, link)? {
                tracing::info!(hold = "screen", screen = ?NoScreen::Unoffered, "hold taken");
            }
            link.control(&ControlToShell::CaptureFailed {
                capture_for: CaptureFor::Gesture(*gesture),
                reason: CaptureFailure::Unavailable,
            })?;
            return Ok(true);
        }
        match message {
            ControlToBrain::VoiceHoldRecognized {
                gesture,
                thread,
                selection,
            } => {
                if self.admit_hold(key(*gesture), *thread, *selection, Kind::Voice, link)? {
                    tracing::info!(hold = "voice", "hold taken");
                }
            }
            ControlToBrain::ScreenHoldRecognized {
                gesture,
                thread,
                selection,
                notice,
                ..
            } => {
                let held = key(*gesture);
                if self.admit_hold(held, *thread, *selection, Kind::Screen, link)? {
                    let screen = self.hold_arm(held, *notice);
                    tracing::info!(hold = "screen", ?screen, "hold taken");
                }
            }
            ControlToBrain::ScreenRelease { gesture, .. } => {
                self.screen_release(key(*gesture), message, link)?;
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
                    let Some(accepted) = self.holds.stopped(key) else {
                        self.holds.unacked = Some(key);
                        return Ok(true);
                    };
                    if self.holds.unacked == Some(key) {
                        self.holds.unacked = None;
                    }
                    link.control(&ControlToShell::VoiceHoldStopAccepted { gesture: *gesture })?;
                    if !accepted
                        && self
                            .holds
                            .pending
                            .as_ref()
                            .is_none_or(|hold| hold.key != key || hold.ask.is_some())
                    {
                        return Ok(true);
                    }
                }
                let Some(hold) = self.holds.pending.as_mut().filter(|hold| hold.key == key) else {
                    return Ok(true);
                };
                if Instant::now() >= hold.deadline {
                    self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link)?;
                    return Ok(true);
                }
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
            ControlToBrain::ExclusionLapsed { lease } if self.holds.lease_lapsed(*lease) => {}
            ControlToBrain::Settings(SettingsOp::SetTalkBack(_) | SettingsOp::SetShowScreen(_))
                if link.registration.is_some() =>
            {
                self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
                return Ok(false);
            }
            ControlToBrain::Settings(SettingsOp::SetDefaultModel(_))
            | ControlToBrain::AccountOp(_)
            | ControlToBrain::Connection(_) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
                let effects = self.coordinator.cancel_current_hold();
                self.apply(effects, Some(link)).map_err(|_| Full)?;
                return Ok(false);
            }
            ControlToBrain::Settings(
                SettingsOp::RemoveVoice(kumochi_protocol::voice::VoicePart::SpeechInput)
                | SettingsOp::SetHoldToTalk(false),
            ) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::NotLoaded), link)?;
                return Ok(false);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Admits the hold `key` as the pending one, or answers why not. True when it's admitted.
    fn admit_hold(
        &mut self,
        key: HoldKey,
        thread: Option<ThreadId>,
        selection: u64,
        kind: Kind,
        link: &mut Link<'_, '_>,
    ) -> Result<bool, Full> {
        if key.gesture.0 <= self.holds.high {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?Refusal::StaleGesture, "held speech input refused");
            return Ok(false);
        }
        self.holds.high = key.gesture.0;
        if !self.hold_pair_supported(link) {
            Self::hold_status(key, HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
            return Ok(false);
        }
        if !self.hold_to_talk_enabled() {
            Self::hold_status(key, HoldStatus::Failed(HoldFailure::NotLoaded), link)?;
            return Ok(false);
        }
        self.holds.retire_exited_peer();
        if self.holds.debt.is_some_and(|debt| debt.epoch != self.epoch) {
            tracing::warn!(stage = ?FailureStage::ReleaseDebt, reason = ?HoldFailure::Unavailable, "held speech input failed");
            Self::hold_status(key, HoldStatus::Failed(HoldFailure::Unavailable), link)?;
            return Ok(false);
        }
        if self.holds.pending.is_some() || self.holds.debt.is_some() || self.changing_model() {
            Self::hold_status(key, HoldStatus::Failed(HoldFailure::Busy), link)?;
            return Ok(false);
        }
        let (state, _) = self.hold_voice();
        if !matches!(state, PartState::Ready | PartState::Loading) {
            tracing::warn!(stage = ?FailureStage::VoiceAvailability, reason = ?state, "held speech input failed");
            let failure = if state == PartState::Unavailable {
                HoldFailure::Unavailable
            } else {
                HoldFailure::NotLoaded
            };
            Self::hold_status(key, HoldStatus::Failed(failure), link)?;
            return Ok(false);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let prepared = self.coordinator.prepare_hold(
            key,
            thread,
            selection,
            self.snapshot(Origin::Kumo),
            Arc::clone(&cancel),
        );
        let Ok(op) = prepared else {
            Self::hold_status(key, HoldStatus::Failed(HoldFailure::ChoiceChanged), link)?;
            return Ok(false);
        };
        self.holds.pending = Some(Hold {
            key,
            kind,
            thread,
            cancel,
            snapshot: None,
            audio: None,
            ask: None,
            released: false,
            deadline: Instant::now() + crate::recall::control::READ_TIME,
            decode: None,
            terminal: None,
            screen: ScreenPart::None,
            screen_recipient: None,
            armed_notice: None,
            words: None,
        });
        if state == PartState::Loading {
            Self::hold_status(key, HoldStatus::Loading, link)?;
        }
        self.apply(vec![Effect::Store(op)], Some(link))
            .map_err(|_| Full)?;
        Ok(true)
    }

    pub(super) fn hold_prepared(
        &mut self,
        key: HoldKey,
        result: Result<HoldSnapshot, ()>,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        let Some(hold) = self.holds.pending.as_mut().filter(|hold| hold.key == key) else {
            tracing::debug!(stage = ?FailureStage::Preparation, reason = ?Refusal::StaleCompletion, "held speech input refused");
            return Ok(());
        };
        if Instant::now() >= hold.deadline {
            return self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link);
        }
        match result {
            Ok(snapshot) if !hold.cancel.load(Ordering::SeqCst) => hold.snapshot = Some(snapshot),
            _ => {
                tracing::debug!(stage = ?FailureStage::Preparation, reason = ?Refusal::ReadOrCancellation, "held speech input refused");
                return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
            }
        }
        hold.deadline = Instant::now() + crate::speech::DEADLINES.load;
        let choice = hold
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.defaults.choice.clone());
        let kind = hold.kind;
        // The gate passed for the default model; the thread may pin another.
        if !matches!(hold.screen, ScreenPart::None) {
            let gate = if kind == Kind::Screen {
                self.display_gate(choice.as_ref())
            } else {
                self.screen_gate(choice.as_ref())
            };
            match gate {
                ScreenHolds::On { .. } => {}
                // Switched off by the user, so the hold goes on as voice with no line.
                ScreenHolds::Off(ScreenOff::TurnedOff) => self.holds.close_screen_as(None),
                ScreenHolds::Off(ScreenOff::ModelCantSee) => {
                    self.holds.close_screen_as(Some(ScreenClass::ModelCantSee));
                }
                ScreenHolds::Off(ScreenOff::NotReady | ScreenOff::Unsupported) => {
                    self.holds.close_screen_as(Some(ScreenClass::NotReady));
                }
            }
        }
        self.hold_voice_changed(link)
    }

    pub(super) fn hold_voice_changed(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        let Some(hold) = &self.holds.pending else {
            return Ok(());
        };
        if hold.cancel.load(Ordering::SeqCst) {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        // Its words are in, so the voice part no longer matters; the run's admission still does.
        if hold.words.is_some() {
            return Ok(());
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
            tracing::warn!(stage = ?FailureStage::VoiceAvailability, reason = ?state, "held speech input failed");
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
        self.holds.debt_peer = self.holds.peer.clone();
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
            tracing::debug!(stage = ?FailureStage::Decode, reason = ?Refusal::StaleCompletion, "held speech input refused");
            return Ok(());
        }
        // A screen answer releases its lease whatever the hold's state, so it skips both ends.
        let screen = matches!(event, Event::Screen(_));
        // Words that waited for a picture until its ticket's deadline go on without it.
        if !screen
            && let Some(hold) = self.holds.pending.as_mut().filter(|hold| {
                !hold.cancel.load(Ordering::SeqCst) && Instant::now() >= hold.deadline
            })
            && let Some(text) = hold.words.take()
        {
            return self.hold_submit(text, link);
        }
        if !screen
            && self
                .holds
                .pending
                .as_ref()
                .is_some_and(|hold| hold.cancel.load(Ordering::SeqCst))
        {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        if !screen
            && self
                .holds
                .pending
                .as_ref()
                .is_some_and(|hold| Instant::now() >= hold.deadline)
        {
            return self.hold_end(HoldStatus::Failed(HoldFailure::TimedOut), link);
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
                // The shell keeps a copy until the turn is stored; longer words aren't kept.
                if text.len() <= MAX_TRANSCRIPT_BYTES
                    && let Some(hold) = &self.holds.pending
                {
                    link.data(&DataToShell::HoldWords {
                        gesture: hold.key.gesture,
                        words: text.as_str().into(),
                    })?;
                }
                // A screen hold's words wait for the picture it's still capturing, until the
                // ticket's deadline.
                if let Some(hold) = self.holds.pending.as_mut()
                    && let ScreenPart::Capturing { until, .. } = hold.screen
                {
                    hold.deadline = until;
                    hold.words = Some(text);
                    #[cfg(test)]
                    self.observer.send(super::Seen::WordsWaiting).ok();
                    return Ok(());
                }
                self.hold_submit(text, link)
            }
            Event::Decoded(_, SttResult::NotLoaded | SttResult::Stale) => {
                self.hold_end(HoldStatus::Failed(HoldFailure::NotLoaded), link)
            }
            Event::Decoded(_, SttResult::Failed(reason)) => {
                tracing::warn!(stage = ?FailureStage::Decode, ?reason, "held speech input failed");
                self.hold_end(HoldStatus::Failed(HoldFailure::DecodeFailed), link)
            }
            Event::Screen(event) => self.hold_screen_event(event, link),
        }
    }

    /// Hands the pending hold's transcript and screen to its run.
    fn hold_submit(&mut self, text: String, link: &mut Link<'_, '_>) -> Result<(), Full> {
        if !self.hold_pair_supported(link) {
            return self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link);
        }
        // shortcut: busy transfer discards the hold; retain its bounded cancellable owner until admission opens when queued voice is needed.
        if !self.admits_turn(link) {
            return self.hold_end(HoldStatus::Failed(HoldFailure::Busy), link);
        }
        let screen = self.holds.submit_screen();
        let Some(mut hold) = self.holds.take() else {
            return Ok(());
        };
        let snapshot = hold.snapshot.take().expect("decode has a pinned snapshot");
        match self.coordinator.submit_hold(snapshot, text.into(), screen) {
            Ok(effects) => self.apply(effects, Some(link)).map_err(|_| Full),
            Err(()) => Self::hold_status(
                hold.key,
                HoldStatus::Failed(HoldFailure::ChoiceChanged),
                link,
            ),
        }
    }

    /// Whether a hold on `choice` may show the model the window under the pointer now, or why not.
    fn screen_gate(&self, choice: Option<&ModelChoice>) -> ScreenHolds {
        self.picture_gate(choice, self.config.view().screen_holds)
    }

    /// The user turned screen holds off: a pending window hold goes on as voice with no line, and
    /// every run's window image is withdrawn. A screen hold's picture follows its own setting.
    pub(super) fn screen_holds_off(&mut self) {
        if self
            .holds
            .pending
            .as_ref()
            .is_some_and(|hold| hold.kind != Kind::Screen)
        {
            self.holds.close_screen_as(None);
        }
        self.offline(Input::ScreenHoldsOff);
    }

    /// The user turned showing the screen off: a pending screen hold goes on as voice with no
    /// line, and every run's screen picture is withdrawn.
    pub(super) fn show_screen_off(&mut self) {
        if self
            .holds
            .pending
            .as_ref()
            .is_some_and(|hold| hold.kind == Kind::Screen)
        {
            self.holds.close_screen_as(None);
        }
        self.offline(Input::ShowScreenOff);
    }

    /// Claude Code's generation moved: every picture held for it that wasn't granted is lost, so
    /// the pending hold's closes and the current run's never goes. Text goes on.
    pub(super) fn claude_generation_moved(&mut self) {
        if !self.claude.take_picture_loss() {
            return;
        }
        if self.holds.pending.as_ref().is_some_and(|hold| {
            !matches!(
                hold.screen,
                ScreenPart::None | ScreenPart::Closed(_) | ScreenPart::Discarded
            ) && hold
                .screen_recipient
                .as_ref()
                .is_some_and(|(kind, _)| *kind == ConnectionKind::ClaudeCode)
        }) {
            self.holds.close_screen_as(Some(ScreenClass::ModelCantSee));
        }
        self.coordinator
            .lose_pictures(Some(ConnectionKind::ClaudeCode));
        if self.holds.screen_sent.as_ref().is_some_and(|(state, _)| {
            matches!(
                state,
                ScreenHolds::On {
                    connection: ConnectionKind::ClaudeCode,
                    ..
                }
            )
        }) {
            self.holds.screen_sent = None;
        }
    }

    /// Sends the gate's state for the default model once per connection, then whenever it
    /// changes. It speaks for the screen's picture at release, which is the hold the shell sends.
    pub(super) fn screen_holds_changed(&mut self, link: &mut Link<'_, '_>) -> Result<(), Full> {
        // A pending hold's picture closes once its own recipient stops taking pictures, so the
        // same capture can't regain permission if the fact comes back; the voice turn goes on.
        if self.holds.pending.as_ref().is_some_and(|hold| {
            !matches!(
                hold.screen,
                ScreenPart::None | ScreenPart::Closed(_) | ScreenPart::Discarded
            ) && hold
                .screen_recipient
                .as_ref()
                .is_some_and(|(kind, model)| !self.coordinator.takes_pictures(*kind, model))
        }) {
            self.holds.close_screen_as(Some(ScreenClass::ModelCantSee));
        }
        let (state, tag) = self.offered();
        let offer = |state: &ScreenHolds| match state {
            ScreenHolds::On {
                connection, slug, ..
            } => Some((*connection, slug.clone())),
            ScreenHolds::Off(_) => None,
        };
        let sent = self.holds.screen_sent.as_ref();
        if sent.is_some_and(|sent| sent.0 == state && sent.1 == tag) {
            return Ok(());
        }
        // Only a change in what the offer authorizes counts the revision up; a new label doesn't.
        let same = sent.is_some_and(|sent| sent.1 == tag && offer(&sent.0) == offer(&state));
        let notice = match self.holds.notice {
            Some(notice) if same => notice,
            last => last.map_or(NoticeRevision::FIRST, NoticeRevision::next),
        };
        self.holds.notice = Some(notice);
        let mut stamped = state.clone();
        if let ScreenHolds::On { revision, .. } = &mut stamped {
            *revision = notice;
        }
        link.control(&ControlToShell::ScreenHolds(stamped))?;
        self.holds.screen_sent = Some((state, tag));
        Ok(())
    }

    /// The offer for the default model now, and the sign-in it goes on.
    fn offered(&self) -> (ScreenHolds, Option<crate::context::ConnectionTag>) {
        let choice = self.config.view().default_model.clone();
        let state = self.display_gate(choice.as_ref());
        let tag =
            choice.and_then(
                |choice| match self.coordinator.availability_of(choice.connection) {
                    crate::coordinator::Availability::Ready(ready) => Some(ready.connection),
                    _ => None,
                },
            );
        (state, tag)
    }

    /// Whether `notice`, the offer a hold's first input was seen under, is the `On` last sent and
    /// still says what the brain would send now. None, an older revision or a moved authority
    /// never is, though the offer may look the same again.
    pub(super) fn offer_holds(&self, notice: Option<NoticeRevision>) -> bool {
        notice.is_some()
            && notice == self.holds.notice
            && self.holds.screen_sent.as_ref().is_some_and(|sent| {
                matches!(sent.0, ScreenHolds::On { .. }) && *sent == self.offered()
            })
    }

    /// Takes a screen task's answer. The capture's end, either way, releases the shell's
    /// exclusion lease for that hold on this connection, and words waiting for its picture go on.
    fn hold_screen_event(
        &mut self,
        event: ScreenEvent,
        link: &mut Link<'_, '_>,
    ) -> Result<(), Full> {
        let capture_for = CaptureFor::Gesture(event.key.gesture);
        let unsupported = matches!(event.outcome, Outcome::Unsupported);
        // Only ever cleared, so nothing later in the session offers screen holds again.
        if unsupported {
            self.holds.window_capture = false;
        }
        let lease = match event.outcome {
            _ if event.key.epoch != self.epoch => None,
            Outcome::Held => Some(ControlToShell::CaptureDone { capture_for }),
            Outcome::Closed(_) | Outcome::Unsupported if event.generation.is_none() => {
                Some(ControlToShell::CaptureFailed {
                    capture_for,
                    reason: CaptureFailure::Unavailable,
                })
            }
            Outcome::Sealed(_) | Outcome::Closed(_) | Outcome::Unsupported => None,
        };
        #[cfg(test)]
        let sealed = matches!(event.outcome, Outcome::Sealed(_));
        self.holds.screen_event(event);
        #[cfg(test)]
        if sealed {
            self.observer.send(super::Seen::PictureSealed).ok();
        }
        if let Some(message) = lease {
            link.control(&message)?;
        }
        if unsupported {
            self.screen_holds_changed(link)?;
        }
        let waited = self
            .holds
            .pending
            .as_mut()
            .filter(|hold| !capturing(&hold.screen))
            .and_then(|hold| {
                let cancelled = hold.cancel.load(Ordering::SeqCst);
                hold.words.take().map(|text| (text, cancelled))
            });
        match waited {
            None => Ok(()),
            Some((_, true)) => self.hold_end(HoldStatus::Failed(HoldFailure::ChoiceChanged), link),
            Some((text, false)) => self.hold_submit(text, link),
        }
    }
}
