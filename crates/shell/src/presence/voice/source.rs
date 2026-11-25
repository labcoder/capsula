use std::sync::atomic::Ordering;
use std::time::Duration;

use kumochi_protocol::message::{ControlToBrain, DataToBrain};
use kumochi_shell_core::link::output::Denied;
use zeroize::Zeroize as _;

use super::inhibit::RECHECK;
use super::{Context, FailureReason, FailureStage, Presence, ReceiptStep};
use crate::platform::microphone::{Finalization, MicOwner, Mode, Reserved, StopReason};
use crate::strings::Text;

impl Presence {
    #[expect(
        clippy::too_many_lines,
        reason = "one source keeps acceptance, forwarding and physical completion together"
    )]
    /// Starts the admitted hold's microphone on the start its waiter reserved and checked.
    pub(super) fn open_microphone(&mut self, reserved: Reserved, cx: &mut Context<'_, Self>) {
        let hold = self.voice.state.pending.as_ref().expect("admitted hold");
        let (token, session, stamp) = (hold.token().clone(), hold.session().clone(), hold.stamp());
        #[cfg(test)]
        let source = if let Some(start) = self.voice.start.take() {
            start(reserved, session.alive())
        } else {
            assert!(
                !cx.has_global::<super::super::test_fixture::WithoutDesktop>(),
                "this test must inject its microphone source"
            );
            MicOwner::start_reserved(reserved, session.alive())
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
        self.voice.state.record();
        let (link, gates) = (self.link.clone(), self.voice.gates.clone());
        let (watch, alive) = (token.clone(), session.alive());
        let drain = session.shell_tuning().drain();
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
                    let current =
                        || this.voice_current(stamp) && this.link.input_is(watch.gesture());
                    if let Some(notice) = this.voice.state.near_limit(&watch, &alive, now, current)
                    {
                        this.voice_notice(notice, cx);
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
                    cx.background_executor().timer(drain).await;
                    let _applied = this.update(cx, |this, cx| {
                        if !watch.retired() && this.voice.state.recording(watch.gesture()) {
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
                        if accepted {
                            // A release or an end while the copies waited wins: none open.
                            this.voice.state.copies()?;
                        } else {
                            gates.claimed(gesture, |claim| output.accept(claim, &mic, running))?;
                            accepted = true;
                            this.voice.state.accepted()?;
                            #[cfg(test)]
                            if let Some(publish) = this.voice.publish.as_mut() {
                                publish();
                            }
                            this.voice_notice(Text::HoldListening, cx);
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
                    if this.voice.state.owns(token.gesture()) {
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
                && !link.is_quitting()
                && !link.send_control(&session, ControlToBrain::VoiceHoldStopped { gesture })
            {
                tracing::warn!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::StopSend, "held speech input failed");
                link.end(&session);
            }
            let receipt = receipt.map(|receipt| receipt.finalization);
            let _applied = this.update(cx, |this, cx| {
                let step = this.voice.state.receipt(gesture, receipt, count);
                match receipt {
                    _ if step == ReceiptStep::Stale => {
                        tracing::debug!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::StaleReceipt, "held speech input refused");
                    }
                    Err(reason) => {
                        tracing::warn!(stage = ?FailureStage::CaptureReceipt, ?reason, "held speech input failed");
                    }
                    Ok(Finalization::Discarded(reason)) => {
                        tracing::warn!(stage = ?FailureStage::CaptureReceipt, ?reason, "held speech input ended");
                    }
                    Ok(Finalization::Released { packets }) if packets != count => {
                        tracing::warn!(stage = ?FailureStage::CaptureReceipt, reason = ?FailureReason::PacketCountMismatch, "held speech input failed");
                    }
                    Ok(Finalization::Released { .. }) => {}
                }
                match step {
                    ReceiptStep::Stale => {}
                    ReceiptStep::End(notice) => this.end_voice(notice, cx),
                    ReceiptStep::Finish => this.finish_voice(),
                }
            });
        })
        .detach();
    }
}
