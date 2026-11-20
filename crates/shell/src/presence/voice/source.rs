use std::sync::atomic::Ordering;
use std::time::Duration;

use kumochi_protocol::message::{ControlToBrain, DataToBrain, HOLD_DRAIN_TIMEOUT_MS};
use kumochi_shell_core::link::output::Denied;
use zeroize::Zeroize as _;

use super::inhibit::RECHECK;
use super::{Context, FailureReason, FailureStage, Presence, Source};
use crate::platform::microphone::{Finalization, MicOwner, Mode, Reserved, StopReason};
use crate::strings::Text;

impl Presence {
    #[expect(
        clippy::too_many_lines,
        reason = "one source keeps acceptance, forwarding and physical completion together"
    )]
    /// Starts the admitted hold's microphone on the start its waiter reserved and checked.
    pub(super) fn open_microphone(&mut self, reserved: Reserved, cx: &mut Context<'_, Self>) {
        let hold = self.voice.pending.as_ref().expect("admitted hold");
        let (token, session, stamp) = (hold.token.clone(), hold.session.clone(), hold.stamp);
        #[cfg(test)]
        let source = match self.voice.start.take() {
            Some(start) => start(reserved, session.alive()),
            None => MicOwner::start_reserved(reserved, session.alive()),
        };
        #[cfg(not(test))]
        let source = MicOwner::start_reserved(reserved, session.alive());
        let mut mic = match source {
            Ok(mic) => mic,
            Err(reason) => {
                tracing::warn!(stage = ?FailureStage::CaptureStart, ?reason, "held speech input failed");
                self.end_voice(Text::HoldMicrophoneUnavailable, cx);
                return;
            }
        };
        self.voice.pending.as_mut().expect("admitted hold").source = Source::Recording;
        let (link, gates) = (self.link.clone(), self.voice.gates.clone());
        let (watch, alive) = (token.clone(), session.alive());
        let warning = cx.spawn({
            let (watch, alive) = (watch.clone(), session.alive());
            async move |this, cx| {
                let clock = cx.background_executor();
                let limit = watch.hold_deadline();
                let remaining = limit.saturating_duration_since(clock.now());
                clock
                    .timer(remaining.saturating_sub(Duration::from_secs(5)))
                    .await;
                let _applied = this.update(cx, |this, cx| {
                    let now = cx.background_executor().now();
                    if now < limit
                        && this.voice.status == Some(Text::HoldListening)
                        && this.voice_current(stamp)
                        && this.link.input_is(watch.gesture())
                        && watch.mode(&alive, now) == Ok(Mode::Collecting)
                    {
                        this.voice_notice(Text::HoldNearLimit, cx);
                    }
                });
            }
        });
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
            let _warning = warning;
            let mut refused = false;
            if let Ok(running) = mic.running.recv().await {
                let mut accepted = false;
                // The same receipt and worker through every recheck: collected, or drained.
                loop {
                    let collected = this.update(cx, |this, cx| {
                        if !this.voice_current(stamp) || !this.link.input_is(token.gesture()) {
                            return Err(Denied::Stale);
                        }
                        let (gates, owner) = (this.voice.gates.clone(), this.link.clone());
                        let (output, gesture) = (owner.output(), token.gesture());
                        if !accepted {
                            gates.claimed(gesture, |claim| output.accept(claim, &mic, running))?;
                            accepted = true;
                            let hold = this.voice.pending.as_mut().ok_or(Denied::Stale)?;
                            hold.collected = true;
                            #[cfg(test)]
                            if let Some(publish) = this.voice.publish.as_mut() {
                                publish();
                            }
                            this.voice_notice(Text::HoldListening, cx);
                        } else if this.voice.pending.as_ref().is_none_or(|hold| {
                            hold.released.is_some() || hold.terminal.is_some()
                        }) {
                            // A release or an end while the copies waited wins: none open.
                            return Err(Denied::Stale);
                        }
                        // Copies open only once Listening is up, under the same claim.
                        gates.claimed(gesture, |claim| output.collect(claim, &mic, running))
                    });
                    if !matches!(collected, Ok(Err(Denied::Busy))) {
                        refused = !matches!(collected, Ok(Ok(())));
                        break;
                    }
                    cx.background_executor().timer(RECHECK).await;
                }
            }
            if refused {
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
            // The physical proof reaches the gate first, even once the hold is gone.
            gates.received(&token);
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
