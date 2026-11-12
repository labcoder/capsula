use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use kumochi_protocol::message::{
    ControlToBrain, GestureId, HoldFailure, HoldStatus, Phase, RunOrigin, ThreadId,
};

use super::{Context, Presence, Session};
use crate::input::hold::Intent;
use crate::platform::microphone::{Generation, Mode};
use crate::strings::Text;

mod source;

#[derive(Debug)]
enum FailureStage {
    Input,
    Admission,
    CaptureStart,
    CaptureReceipt,
    Release,
    Run,
    Brain,
    Notice,
}

#[derive(Debug)]
enum FailureReason {
    GestureUnavailable,
    DismissPanels,
    CancelDrain,
    RecognitionSend,
    PacketCountMismatch,
    RunBeforeStop,
    RunFailed,
    NoSession,
    PendingHold,
    StaleContext,
    InputFence,
    GenerationNotPreparing,
    NoPendingHold,
    StaleStatus,
    TerminalStatus,
    RunningRejected,
    WarmSend,
    StopSend,
    ReleaseSend,
    StaleReceipt,
}

#[cfg(test)]
use crate::platform::microphone::{Finalization, MicOwner};
#[cfg(test)]
use kumochi_protocol::message::DataToBrain;

#[cfg(test)]
#[path = "voice_source_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) target: (Option<ThreadId>, u64),
    pub(super) session: u64,
    pub(super) choice: u64,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Waiting,
    Recording,
    Joined(u32),
    Sent,
}

pub(super) struct Hold {
    token: Generation,
    stamp: Stamp,
    session: Session,
    source: Source,
    released: Option<u64>,
    terminal: Option<Text>,
}

#[derive(Default)]
pub(super) struct Voice {
    pub(super) target: Rc<Cell<(Option<ThreadId>, u64)>>,
    pub(super) pending: Option<Hold>,
    pub(super) status: Option<Text>,
    #[cfg(test)]
    pub(super) start: Option<Start>,
}

#[cfg(test)]
type Start = Box<
    dyn FnOnce(
        Generation,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<MicOwner, crate::platform::microphone::MicError>,
>;

impl Presence {
    pub(super) fn voice_working(&self) -> bool {
        self.voice
            .pending
            .as_ref()
            .is_some_and(|hold| hold.terminal.is_none())
    }

    pub(super) fn voice_run(
        &mut self,
        origin: Option<RunOrigin>,
        phase: Phase,
        applied: &crate::runs::Applied,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(RunOrigin::Hold { gesture }) = origin else {
            return;
        };
        if !applied.visible {
            return;
        }
        let Some(hold) = self
            .voice
            .pending
            .as_ref()
            .filter(|hold| hold.token.gesture() == gesture && hold.terminal.is_none())
        else {
            return;
        };
        if !matches!(hold.source, Source::Sent) {
            tracing::warn!(stage = ?FailureStage::Run, reason = ?FailureReason::RunBeforeStop, "held speech input failed");
            self.end_voice(Text::HoldFailed, cx);
            if let Some(session) = &self.session {
                self.link.end(session);
            }
            return;
        }
        match phase {
            Phase::Thinking => {
                self.voice_notice(Text::HoldThinking, cx);
                if self.show_voice_answer(cx).is_err() {
                    tracing::error!("can't show the voice answer");
                }
            }
            Phase::Answering | Phase::Done => {
                self.link.microphone_gate().cancel(gesture);
                self.link.clear_input(gesture);
                self.voice.pending = None;
                self.voice.status = None;
                self.refresh_dock(cx);
            }
            Phase::Failed => {
                tracing::warn!(stage = ?FailureStage::Run, reason = ?FailureReason::RunFailed, "held speech input failed");
                self.end_voice(Text::HoldFailed, cx);
            }
            Phase::Interrupted => self.end_voice(Text::HoldCancelled, cx),
            _ => {}
        }
    }

    fn voice_current(&self, stamp: Stamp) -> bool {
        stamp.target == (self.runs.thread(), self.runs.revision())
            && stamp.choice == self.link.choice_revision()
            && self.session.as_ref().is_some_and(|s| {
                s.generation() == stamp.session && s.alive().load(Ordering::Acquire)
            })
    }

    fn voice_notice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        tracing::debug!(stage = ?FailureStage::Notice, reason = ?text, "held speech input notice");
        self.voice.status = Some(text);
        self.refresh_dock(cx);
        if self
            .runs_changed(cx)
            .and_then(|()| self.pointer_changed(cx))
            .is_err()
        {
            tracing::error!("can't update voice input status");
        }
    }

    fn unused_voice(&self, token: &Generation, session: &Session) {
        if (token.discard_unstarted().is_ok() || token.retired())
            && !self.link.send_control(
                session,
                ControlToBrain::VoiceHoldStopped {
                    gesture: token.gesture(),
                },
            )
        {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::StopSend, "held speech input failed");
            self.link.end(session);
        }
    }

    fn drain_voice_cancel(&mut self) -> bool {
        self.runs.take_hold_cancel().is_none_or(|message| {
            self.session.as_ref().is_some_and(|s| {
                let sent = self.link.send_control(s, message);
                if !sent {
                    tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::CancelDrain, "held speech input failed");
                    self.link.end(s);
                }
                sent
            })
        })
    }

    fn end_voice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        let mut notice = text;
        if let Some(mut hold) = self.voice.pending.take() {
            let gesture = hold.token.gesture();
            self.link.microphone_gate().cancel(gesture);
            self.link.clear_input(gesture);
            if matches!(hold.source, Source::Waiting) {
                self.unused_voice(&hold.token, &hold.session);
            } else if matches!(hold.source, Source::Recording) {
                let terminal = *hold.terminal.get_or_insert(text);
                notice = if terminal == Text::HoldUnavailable || text == Text::HoldUnavailable {
                    Text::HoldUnavailable
                } else {
                    Text::HoldStopping
                };
                self.voice.pending = Some(hold);
            }
        }
        self.runs.cancel_hold();
        self.drain_voice_cancel();
        self.voice_notice(notice, cx);
    }

    pub(super) fn cancel_voice(&mut self, cx: &mut Context<'_, Self>) {
        self.link.cancel_input();
        if self.voice.pending.is_some() {
            self.end_voice(Text::HoldCancelled, cx);
        } else if self.voice.status.take().is_some() {
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
        }
        self.drain_voice_cancel();
    }

    pub(super) fn cancel_voice_gesture(&mut self, gesture: GestureId, cx: &mut Context<'_, Self>) {
        if self
            .voice
            .pending
            .as_ref()
            .is_some_and(|hold| hold.token.gesture() == gesture)
        {
            self.end_voice(Text::HoldCancelled, cx);
        }
    }

    pub(super) fn voice_intent(
        &mut self,
        intent: Intent,
        stamp: Stamp,
        qpc: u64,
        cx: &mut Context<'_, Self>,
    ) {
        match intent {
            Intent::Warm(gesture) => {
                if let Some(reason) = self.settings.read(cx).hold_to_talk_notice() {
                    tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
                    return;
                }
                if let Some(session) = self
                    .session
                    .as_ref()
                    .filter(|s| s.generation() == stamp.session)
                {
                    if !self.link.send_control(
                        session,
                        ControlToBrain::KeyDown {
                            gesture,
                            t_qpc: qpc,
                        },
                    ) {
                        tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::WarmSend, "held speech input refused");
                    }
                } else {
                    tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::NoSession, "held speech input refused");
                }
            }
            Intent::Recognized {
                gesture,
                generation,
            } => {
                self.recognize_voice(gesture, generation, stamp, cx);
            }
            Intent::End { gesture, cancelled } => {
                let Some(hold) = self
                    .voice
                    .pending
                    .as_mut()
                    .filter(|hold| hold.token.gesture() == gesture)
                else {
                    tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::NoPendingHold, "held speech input refused");
                    return;
                };
                if cancelled || self.voice.status != Some(Text::HoldListening) {
                    self.end_voice(Text::HoldCancelled, cx);
                } else {
                    hold.released = Some(qpc);
                    self.voice_notice(Text::HoldStopping, cx);
                    self.finish_voice();
                }
            }
            Intent::Unavailable => {
                tracing::warn!(stage = ?FailureStage::Input, reason = ?FailureReason::GestureUnavailable, "held speech input failed");
                self.voice_notice(Text::HoldFailed, cx);
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "admission keeps each fixed refusal diagnostic beside its cleanup"
    )]
    fn recognize_voice(
        &mut self,
        gesture: GestureId,
        generation: Result<Generation, crate::platform::microphone::MicError>,
        stamp: Stamp,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(token) = generation else {
            if let Err(reason) = generation {
                tracing::warn!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            }
            let notice = if self.voice.status == Some(Text::HoldUnavailable) {
                Text::HoldUnavailable
            } else {
                Text::TurnBusy
            };
            self.voice_notice(notice, cx);
            return;
        };
        if let Some(notice) = self.settings.read(cx).hold_to_talk_notice() {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?notice, "held speech input refused");
            if let Some(session) = &self.session {
                self.unused_voice(&token, session);
            } else if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(gesture);
            }
            self.link.clear_input(gesture);
            self.voice.status = Some(notice);
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
            return;
        }
        if self.voice.pending.is_some() {
            self.end_voice(Text::HoldCancelled, cx);
        }
        let Some(session) = self.session.clone() else {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::NoSession, "held speech input refused");
            if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(gesture);
            }
            self.link.clear_input(gesture);
            return;
        };
        if self
            .dismiss_settings(cx)
            .and_then(|()| self.dismiss_threads(cx))
            .and_then(|()| self.dismiss_text(cx))
            .is_err()
        {
            tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::DismissPanels, "held speech input failed");
            self.unused_voice(&token, &session);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        let refused = if self.voice.pending.is_some() {
            Some(FailureReason::PendingHold)
        } else if !self.voice_current(stamp) {
            Some(FailureReason::StaleContext)
        } else if !self.link.input_is(gesture) {
            Some(FailureReason::InputFence)
        } else if token.mode(&session.alive(), Instant::now()) != Ok(Mode::Preparing) {
            Some(FailureReason::GenerationNotPreparing)
        } else {
            None
        };
        if let Some(reason) = refused {
            tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            self.unused_voice(&token, &session);
            self.link.clear_input(gesture);
            return;
        }
        let request = self.runs.begin_hold(gesture);
        let notice = if matches!(request, Err(crate::runs::Refused::Busy)) {
            Text::TurnBusy
        } else {
            Text::HoldFailed
        };
        let sent = match request {
            Err(reason) => {
                tracing::warn!(stage = ?FailureStage::Admission, ?reason, "held speech input failed");
                false
            }
            Ok(_) if !self.drain_voice_cancel() => {
                tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::CancelDrain, "held speech input failed");
                false
            }
            Ok(request) => {
                let sent = self.link.send_control(&session, request);
                if !sent {
                    tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::RecognitionSend, "held speech input failed");
                }
                sent
            }
        };
        if sent {
            self.voice.pending = Some(Hold {
                token,
                stamp,
                session,
                source: Source::Waiting,
                released: None,
                terminal: None,
            });
            self.chat_notice.clear();
            self.error = None;
            self.refusal = None;
            self.voice_notice(Text::HoldPreparing, cx);
        } else {
            self.unused_voice(&token, &session);
            self.runs.cancel_hold();
            self.drain_voice_cancel();
            self.voice_notice(notice, cx);
        }
    }

    fn finish_voice(&mut self) {
        let Some(hold) = self.voice.pending.as_mut() else {
            return;
        };
        if let (Source::Joined(chunks), Some(t_qpc)) = (hold.source, hold.released) {
            hold.source = Source::Sent;
            if !self.link.send_control(
                &hold.session,
                ControlToBrain::KeyUp {
                    gesture: hold.token.gesture(),
                    t_qpc,
                    chunks,
                },
            ) {
                tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ReleaseSend, "held speech input failed");
                self.link.end(&hold.session);
            }
        }
    }

    pub(super) fn voice_status(
        &mut self,
        generation: u64,
        gesture: GestureId,
        status: HoldStatus,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(hold) = self
            .voice
            .pending
            .as_ref()
            .filter(|h| h.stamp.session == generation && h.token.gesture() == gesture)
        else {
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::StaleStatus, "held speech input refused");
            return;
        };
        if hold.terminal.is_some() {
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::TerminalStatus, "held speech input refused");
            return;
        }
        match status {
            HoldStatus::Admitted if matches!(hold.source, Source::Waiting) => {
                self.open_microphone(cx);
            }
            HoldStatus::Admitted => {}
            HoldStatus::Loading => self.voice_notice(Text::VoiceLoading, cx),
            HoldStatus::Transcribing => self.voice_notice(Text::HoldTranscribing, cx),
            HoldStatus::Empty => self.end_voice(Text::HoldEmpty, cx),
            HoldStatus::Cancelled => self.end_voice(Text::HoldCancelled, cx),
            HoldStatus::Failed(reason) => {
                tracing::warn!(stage = ?FailureStage::Brain, ?reason, "held speech input failed");
                self.end_voice(
                    match reason {
                        HoldFailure::NotLoaded => {
                            match self.settings.read(cx).speech_input_status() {
                                text @ (Text::VoiceNotInstalled | Text::VoiceUnloaded) => text,
                                _ => Text::VoiceStatusUnknown,
                            }
                        }
                        HoldFailure::Busy => Text::TurnBusy,
                        _ => Text::HoldFailed,
                    },
                    cx,
                );
            }
        }
    }
}
