use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, GestureId, HOLD_DRAIN_TIMEOUT_MS, HoldFailure, HoldStatus, Phase,
    RunOrigin, ThreadId,
};
use zeroize::Zeroize as _;

use super::{Context, Presence, Session};
use crate::input::hold::Intent;
use crate::platform::microphone::{Finalization, Generation, MicOwner, Mode, StopReason};
use crate::strings::Text;

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
            Phase::Failed => self.end_voice(Text::HoldFailed, cx),
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
            self.link.end(session);
        }
    }

    fn drain_voice_cancel(&mut self) -> bool {
        self.runs.take_hold_cancel().is_none_or(|message| {
            self.session.as_ref().is_some_and(|s| {
                let sent = self.link.send_control(s, message);
                if !sent {
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
                if self.settings.read(cx).hold_to_talk_notice().is_some() {
                    return;
                }
                if let Some(session) = self
                    .session
                    .as_ref()
                    .filter(|s| s.generation() == stamp.session)
                {
                    self.link.send_control(
                        session,
                        ControlToBrain::KeyDown {
                            gesture,
                            t_qpc: qpc,
                        },
                    );
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
            Intent::Unavailable => self.voice_notice(Text::HoldFailed, cx),
        }
    }

    fn recognize_voice(
        &mut self,
        gesture: GestureId,
        generation: Result<Generation, crate::platform::microphone::MicError>,
        stamp: Stamp,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(token) = generation else {
            let notice = if self.voice.status == Some(Text::HoldUnavailable) {
                Text::HoldUnavailable
            } else {
                Text::TurnBusy
            };
            self.voice_notice(notice, cx);
            return;
        };
        if let Some(notice) = self.settings.read(cx).hold_to_talk_notice() {
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
            self.unused_voice(&token, &session);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        if self.voice.pending.is_some()
            || !self.voice_current(stamp)
            || !self.link.input_is(gesture)
            || token.mode(&session.alive(), Instant::now()) != Ok(Mode::Preparing)
        {
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
        if let Ok(request) = request
            && self.drain_voice_cancel()
            && self.link.send_control(&session, request)
        {
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
            return;
        };
        if hold.terminal.is_some() {
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
            HoldStatus::Failed(reason) => self.end_voice(
                match reason {
                    HoldFailure::NotLoaded => match self.settings.read(cx).speech_input_status() {
                        text @ (Text::VoiceNotInstalled | Text::VoiceUnloaded) => text,
                        _ => Text::VoiceStatusUnknown,
                    },
                    HoldFailure::Busy => Text::TurnBusy,
                    _ => Text::HoldFailed,
                },
                cx,
            ),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one source keeps acceptance, forwarding and physical completion together"
    )]
    fn open_microphone(&mut self, cx: &mut Context<'_, Self>) {
        let hold = self.voice.pending.as_ref().expect("admitted hold");
        if !self.voice_current(hold.stamp)
            || !self.link.input_is(hold.token.gesture())
            || hold.token.mode(&hold.session.alive(), Instant::now()) != Ok(Mode::Preparing)
        {
            self.end_voice(Text::HoldCancelled, cx);
            return;
        }
        let (token, session, stamp) = (hold.token.clone(), hold.session.clone(), hold.stamp);
        #[cfg(test)]
        let source = match self.voice.start.take() {
            Some(start) => start(token.clone(), session.alive()),
            None => MicOwner::start(token.clone(), session.alive()),
        };
        #[cfg(not(test))]
        let source = MicOwner::start(token.clone(), session.alive());
        let Ok(mut mic) = source else {
            self.end_voice(Text::HoldMicrophoneUnavailable, cx);
            return;
        };
        self.voice.pending.as_mut().expect("admitted hold").source = Source::Recording;
        let link = self.link.clone();
        let (watch, alive) = (token.clone(), session.alive());
        cx.spawn(async move |this, cx| {
            while !watch.retired() {
                cx.background_executor()
                    .timer(crate::platform::microphone::POLL)
                    .await;
                if !matches!(
                    watch.mode(&alive, cx.background_executor().now()),
                    Ok(Mode::Preparing | Mode::Collecting)
                ) {
                    cx.background_executor()
                        .timer(Duration::from_millis(u64::from(HOLD_DRAIN_TIMEOUT_MS)))
                        .await;
                    let _applied = this.update(cx, |this, cx| {
                        if !watch.retired()
                            && this.voice.pending.as_ref().is_some_and(|hold| {
                                hold.token.gesture() == watch.gesture()
                                    && matches!(hold.source, Source::Recording)
                            })
                        {
                            this.end_voice(Text::HoldUnavailable, cx);
                        }
                    });
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let running = mic.running.recv().await;
            let accepted = running.as_ref().is_ok_and(|_| {
                this.update(cx, |this, cx| {
                    if !this.voice_current(stamp) || !this.link.input_is(token.gesture()) {
                        return false;
                    }
                    this.voice_notice(Text::HoldListening, cx);
                    true
                })
                .unwrap_or(false)
            });
            if running.is_ok_and(|running| !accepted || mic.accept_running(running).is_err()) {
                mic.stop(StopReason::Cancel);
                let _applied = this.update(cx, |this, cx| {
                    if this
                        .voice
                        .pending
                        .as_ref()
                        .is_some_and(|hold| hold.token.gesture() == token.gesture())
                    {
                        this.end_voice(Text::HoldCancelled, cx);
                    }
                });
            }
            let pcm = mic.pcm.clone();
            let (sending, active, alive, gate) = (
                link.clone(),
                session.clone(),
                session.alive(),
                link.microphone_gate(),
            );
            let gesture = token.gesture();
            let forwarded = cx.background_executor().spawn(async move {
                let mut count = 0;
                while let Ok(mut packet) = pcm.recv().await {
                    if sending.input_is(gesture)
                        && alive.load(Ordering::Acquire)
                        && sending.choice_revision() == stamp.choice
                    {
                        if sending.send_data(&active, DataToBrain::MicPcm(packet)) {
                            count += 1;
                            continue;
                        }
                        gate.cancel(gesture);
                        sending.clear_input(gesture);
                    } else {
                        packet.samples.zeroize();
                    }
                }
                count
            });
            let receipt = mic.receipt().await;
            let count = if receipt.is_ok() {
                forwarded.await
            } else {
                drop(forwarded);
                0
            };
            if receipt.is_ok()
                && !link.send_control(&session, ControlToBrain::VoiceHoldStopped { gesture })
            {
                link.end(&session);
            }
            let _applied = this.update(cx, |this, cx| {
                let Some(hold) = this
                    .voice
                    .pending
                    .as_mut()
                    .filter(|h| h.token.gesture() == gesture)
                else {
                    return;
                };
                match receipt {
                    Ok(receipt) => {
                        hold.source = Source::Sent;
                        if let Some(terminal) = hold.terminal {
                            this.end_voice(terminal, cx);
                        } else if receipt.finalization
                            == (Finalization::Released { packets: count })
                        {
                            hold.source = Source::Joined(count);
                            this.finish_voice();
                        } else {
                            let notice = match receipt.finalization {
                                Finalization::Discarded(
                                    crate::platform::microphone::MicError::Cancelled
                                    | crate::platform::microphone::MicError::SessionLost,
                                ) => Text::HoldCancelled,
                                Finalization::Discarded(
                                    crate::platform::microphone::MicError::Unavailable
                                    | crate::platform::microphone::MicError::Format,
                                ) => Text::HoldMicrophoneUnavailable,
                                _ => Text::HoldFailed,
                            };
                            this.end_voice(notice, cx);
                        }
                    }
                    Err(_) => this.end_voice(Text::HoldUnavailable, cx),
                }
            });
        })
        .detach();
    }
}
