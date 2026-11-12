use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{ControlToBrain, DataToBrain, HOLD_DRAIN_TIMEOUT_MS};
use zeroize::Zeroize as _;

use super::{Context, FailureReason, FailureStage, Presence, Source};
use crate::platform::microphone::{Finalization, MicOwner, Mode, StopReason};
use crate::strings::Text;

impl Presence {
    #[expect(
        clippy::too_many_lines,
        reason = "one source keeps acceptance, forwarding and physical completion together"
    )]
    pub(super) fn open_microphone(&mut self, cx: &mut Context<'_, Self>) {
        let hold = self.voice.pending.as_ref().expect("admitted hold");
        if !self.voice_current(hold.stamp)
            || !self.link.input_is(hold.token.gesture())
            || hold.token.mode(&hold.session.alive(), Instant::now()) != Ok(Mode::Preparing)
        {
            tracing::debug!(stage = ?FailureStage::CaptureStart, reason = ?FailureReason::StaleContext, "held speech input refused");
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
        let mut mic = match source {
            Ok(mic) => mic,
            Err(reason) => {
                tracing::warn!(stage = ?FailureStage::CaptureStart, ?reason, "held speech input failed");
                self.end_voice(Text::HoldMicrophoneUnavailable, cx);
                return;
            }
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
                tracing::debug!(stage = ?FailureStage::CaptureStart, reason = ?FailureReason::RunningRejected, "held speech input refused");
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
                tracing::warn!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::StopSend, "held speech input failed");
                link.end(&session);
            }
            let _applied = this.update(cx, |this, cx| {
                let Some(hold) = this
                    .voice
                    .pending
                    .as_mut()
                    .filter(|h| h.token.gesture() == gesture)
                else {
                    tracing::debug!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::StaleReceipt, "held speech input refused");
                    return;
                };
                match receipt {
                    Ok(receipt) => {
                        match receipt.finalization {
                            Finalization::Discarded(reason) => {
                                tracing::warn!(stage = ?FailureStage::CaptureReceipt, ?reason, "held speech input ended");
                            }
                            Finalization::Released { packets } if packets != count => {
                                tracing::warn!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::PacketCountMismatch, "held speech input failed");
                            }
                            Finalization::Released { .. } => {}
                        }
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
                        hold.source = Source::Sent;
                        if let Some(terminal) = hold.terminal {
                            this.end_voice(
                                if terminal == Text::HoldUnavailable {
                                    notice
                                } else {
                                    terminal
                                },
                                cx,
                            );
                        } else if receipt.finalization
                            == (Finalization::Released { packets: count })
                        {
                            hold.source = Source::Joined(count);
                            this.finish_voice();
                        } else {
                            this.end_voice(notice, cx);
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(stage = ?FailureStage::CaptureReceipt, ?reason, "held speech input failed");
                        this.end_voice(Text::HoldUnavailable, cx);
                    }
                }
            });
        })
        .detach();
    }
}
