use std::cell::Cell;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, MAX_VISIBLE_SCENES, OutputGeneration, SettingsOp,
    check_rate,
};
use kumochi_protocol::tap::{Direction, Tapped};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Brain, Ended, Full, Link, close_wait};
use crate::connections::account::Input as AccountInput;
use crate::coordinator::Input;
use crate::ipc;
use crate::tap::Tap;
use crate::voice::installer::Installer;
use crate::voice::pump::{OutputMode, Pump};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Registration {
    Awaiting,
    Applied(OutputGeneration),
}

impl Registration {
    pub(super) fn admits(registration: Option<&Cell<Self>>) -> bool {
        registration.is_none_or(|state| matches!(state.get(), Self::Applied(_)))
    }

    fn control(self, message: &ControlToBrain) -> Result<Option<(u32, OutputGeneration)>, Ended> {
        match message {
            ControlToBrain::QueuedAudioOutput(rate, generation) => {
                if self != Self::Awaiting {
                    return Err(Ended::Broken("queued audio registered twice"));
                }
                if generation.0 == 0 || check_rate(*rate).is_err() {
                    return Err(Ended::Broken("invalid queued audio output"));
                }
                Ok(Some((*rate, *generation)))
            }
            ControlToBrain::AudioOutput { .. } => {
                Err(Ended::Broken("legacy audio on a queued connection"))
            }
            ControlToBrain::AudioOutputUnavailable { generation }
                if self == Self::Awaiting || self == Self::Applied(*generation) =>
            {
                Err(Ended::Broken("queued audio output withdrawn"))
            }
            ControlToBrain::KeyDown { .. }
            | ControlToBrain::VoiceHoldRecognized { .. }
            | ControlToBrain::ScreenHoldRecognized { .. }
                if self == Self::Awaiting =>
            {
                Err(Ended::Broken("hold before queued audio registration"))
            }
            ControlToBrain::HoldRecognized { .. } => Err(Ended::Broken(match self {
                Self::Awaiting => "hold before queued audio registration",
                Self::Applied(_) => "hold on a Plain queued connection",
            })),
            _ => Ok(None),
        }
    }
}

pub(super) async fn read_control(
    pipe: &mut (impl AsyncRead + Unpin),
    frames: &mpsc::Sender<(ControlToBrain, usize)>,
    quit: &Cell<Option<Instant>>,
) -> Ended {
    loop {
        match ipc::read_sized::<ControlToBrain>(pipe, Pipe::Control).await {
            Ok(Some((ControlToBrain::Hello(_) | ControlToBrain::QueuedHello(_), _))) => {
                return Ended::Broken("Hello sent twice");
            }
            Ok(Some((ControlToBrain::Quitting, _))) => {
                let deadline = Instant::now() + close_wait();
                quit.set(Some(deadline));
                return Ended::Quit { deadline };
            }
            Ok(Some(sized)) => {
                if frames.send(sized).await.is_err() {
                    return Ended::Lost;
                }
            }
            Ok(None) => return Ended::Lost,
            Err(error) => {
                tracing::warn!(%error, "control pipe failed");
                return Ended::Lost;
            }
        }
    }
}

#[expect(clippy::too_many_lines, reason = "one flat arm per control message")]
pub(super) fn control_input(message: ControlToBrain) -> Result<Option<Input>, Ended> {
    Ok(match message {
        ControlToBrain::Stop { run, revealed } => Some(Input::Stop { run, revealed }),
        ControlToBrain::ClearDrawing { scene_ids } if scene_ids.len() > MAX_VISIBLE_SCENES => {
            return Err(Ended::Broken("more scenes cleared than can be visible"));
        }
        ControlToBrain::ClearDrawing { scene_ids } => Some(Input::Clear { scenes: scene_ids }),
        ControlToBrain::SceneFailed { scene } => {
            Some(Input::Drawing(crate::drawing::Input::SceneFailed { scene }))
        }
        ControlToBrain::SceneSkipped { scene } => {
            Some(Input::Drawing(crate::drawing::Input::SceneSkipped {
                scene,
            }))
        }
        ControlToBrain::StepReady { run, scene, index } => {
            Some(Input::Drawing(crate::drawing::Input::StepReady {
                run,
                scene,
                index,
            }))
        }
        ControlToBrain::DeleteThread {
            thread,
            after_turns,
            resend,
        } => Some(Input::Delete {
            thread,
            after_turns,
            resend,
        }),
        ControlToBrain::ThreadOp(op) => Some(Input::ThreadOp(op)),
        ControlToBrain::StopScenes { run, cuts } => Some(Input::StopScenes { run, cuts }),
        ControlToBrain::StepShown { run, scene, index } => {
            Some(Input::Drawing(crate::drawing::Input::StepShown {
                run,
                scene,
                index,
            }))
        }
        ControlToBrain::StepFailed {
            run,
            scene,
            index,
            reason,
            cut,
        } => Some(Input::Drawing(crate::drawing::Input::StepFailed {
            run,
            scene,
            index,
            reason,
            cut,
        })),
        // shortcut: B2a admits holds and consumes focus hints and exclusion lease changes.
        message @ (ControlToBrain::HoldRecognized { .. }
        | ControlToBrain::VoiceHoldRecognized { .. }
        | ControlToBrain::VoiceHoldStopped { .. }
        | ControlToBrain::KeyDown { .. }
        | ControlToBrain::KeyUp { .. }
        | ControlToBrain::GestureCancel { .. }
        | ControlToBrain::FocusHint { .. }
        | ControlToBrain::ExclusionLapsed { .. }
        | ControlToBrain::ScreenHoldRecognized { .. }
        | ControlToBrain::ScreenRelease { .. }) => {
            tracing::debug!(op = message.op(), "hold control ignored");
            None
        }
        ControlToBrain::AudioOutput { rate, .. } if check_rate(rate).is_err() => {
            return Err(Ended::Broken("an output rate out of bounds"));
        }
        ControlToBrain::AudioOutput { rate, generation } => {
            Some(Input::AudioOutput { rate, generation })
        }
        ControlToBrain::QueuedAudioOutput(rate, generation) => check_rate(rate)
            .map(|()| Some(Input::QueuedAudioOutput(rate, generation)))
            .map_err(|_| Ended::Broken("an output rate out of bounds"))?,
        ControlToBrain::AudioOutputUnavailable { generation } => {
            Some(Input::AudioOutputUnavailable { generation })
        }
        ControlToBrain::SpeechProgress {
            run,
            segment,
            sample,
            ..
        } => Some(Input::SpeechProgress {
            run,
            segment,
            sample,
        }),
        ControlToBrain::SpeechStopped { run, cursor } => Some(Input::SpeechStopped { run, cursor }),
        ControlToBrain::SpeechRetired(receipt) => Some(Input::SpeechRetired(receipt)),
        ControlToBrain::SpeechDecision(key, decision) => Some(Input::SpeechDecision(key, decision)),
        ControlToBrain::SegmentPlayed {
            run,
            segment,
            samples,
        } => Some(Input::SegmentPlayed {
            run,
            segment,
            samples,
        }),
        message @ (ControlToBrain::GitHub(_) | ControlToBrain::Issue { .. }) => {
            tracing::debug!(op = message.op(), "github or issue control ignored");
            None
        }
        // shortcut: nothing answers Connection or Warm until A1, C2 and C5b build them.
        ControlToBrain::Pong
        | ControlToBrain::Hello(_)
        | ControlToBrain::QueuedHello(_)
        | ControlToBrain::Quitting
        | ControlToBrain::Settings(_)
        | ControlToBrain::AccountOp(_)
        | ControlToBrain::Connection(_)
        | ControlToBrain::Warm { .. } => None,
    })
}

impl Brain {
    pub(super) fn control_after_loss(
        &mut self,
        message: ControlToBrain,
        len: usize,
        tap: &mut Tap,
        audio: AudioSession,
    ) {
        if audio == AudioSession::Legacy
            || !matches!(
                message,
                ControlToBrain::AudioOutput { .. }
                    | ControlToBrain::QueuedAudioOutput(..)
                    | ControlToBrain::AudioOutputUnavailable { .. }
            )
        {
            self.control_offline(message, len, tap);
        }
    }

    /// Applies a control frame with no shell to tell, as `serve` does once the core has gone.
    pub(super) fn control_offline(&mut self, message: ControlToBrain, len: usize, tap: &mut Tap) {
        tap.record(Pipe::Control, Direction::ToBrain, &message, len);
        if let ControlToBrain::VoiceHoldStopped { gesture } = &message {
            self.holds.stopped(crate::coordinator::HoldKey {
                epoch: self.epoch,
                gesture: *gesture,
            });
            return;
        }
        if let ControlToBrain::Issue {
            session,
            request,
            op,
        } = message
        {
            if let Some(effects) = self.issue_op(session, request, &op, None) {
                self.apply(effects, None)
                    .expect("offline issue effects cannot fill a shell queue");
            }
            return;
        }
        if let ControlToBrain::AccountOp(op) = message {
            let applied = self.account_input(AccountInput::Op(op), None);
            debug_assert!(applied.is_ok(), "applying without a shell can't fail");
            return;
        }
        if let ControlToBrain::Connection(op) = message {
            let applied = self.connection_op(op, None);
            debug_assert!(applied.is_ok(), "applying without a shell can't fail");
            return;
        }
        if let ControlToBrain::Settings(op) = message {
            // Still written; the shell that sent it is gone, so nothing answers it.
            self.queue_settings(op);
            return;
        }
        if let Ok(Some(input)) = control_input(message) {
            #[cfg(test)]
            self.control_taken(&input);
            self.offline(input);
        }
    }

    /// A control frame from the connected shell: recorded, then a settings change, a pong, or
    /// an input for the coordinator.
    pub(super) fn control_frame(
        &mut self,
        message: ControlToBrain,
        len: usize,
        link: &mut Link<'_, '_>,
        ping_sent: &mut Option<Instant>,
    ) -> Result<Option<Input>, Ended> {
        link.tap
            .record(Pipe::Control, Direction::ToBrain, &message, len);
        if let Some(registration) = link.registration
            && let Some((rate, output)) = registration.get().control(&message)?
        {
            // shortcut: the pump's mode stands in for a conductor; P3-1.4 gives the pair its own.
            let mode = self.pump.as_ref().map_or(OutputMode::Legacy, Pump::output);
            let effects = self
                .coordinator
                .register_queued_output(rate, output, mode)
                .map_err(|_| Ended::Broken("stale queued audio output"))?;
            self.apply(effects, Some(link))?;
            link.control(&ControlToShell::QueuedAudioRegistered(output))
                .map_err(|Full| Ended::Lost)?;
            registration.set(Registration::Applied(output));
            return Ok(None);
        }
        let hold = self.hold_control(&message, link);
        if let ControlToBrain::Settings(op) = message {
            match hold {
                Ok(false) => {}
                Err(Full) if matches!(op, SettingsOp::SetHoldToTalk(false)) => {}
                Ok(true) => return Ok(None),
                Err(Full) => return Err(Ended::Lost),
            }
            // Off closes the offer as it's taken; only a finished save opens it again.
            let off = op == SettingsOp::SetShowScreen(false);
            self.queue_settings(op);
            hold.map_err(|Full| Ended::Lost)?;
            self.pump_settings(Some(link)).map_err(|Full| Ended::Lost)?;
            if off {
                self.screen_holds_changed(link)
                    .map_err(|Full| Ended::Lost)?;
            }
            return Ok(None);
        }
        if hold.map_err(|Full| Ended::Lost)? {
            return Ok(None);
        }
        if message == ControlToBrain::Pong {
            *ping_sent = None;
        }
        if let ControlToBrain::Issue {
            session,
            request,
            op,
        } = message
        {
            let effects = self
                .issue_op(session, request, &op, Some(self.epoch))
                .ok_or(Ended::Lost)?;
            self.apply(effects, Some(link))?;
            return Ok(None);
        }
        if let ControlToBrain::GitHub(op) = message {
            self.github_op(op, Some(link)).map_err(|Full| Ended::Lost)?;
            return Ok(None);
        }
        if let ControlToBrain::AccountOp(op) = message {
            self.account_input(AccountInput::Op(op), Some(link))
                .map_err(|Full| Ended::Lost)?;
            self.screen_holds_changed(link)
                .map_err(|Full| Ended::Lost)?;
            return Ok(None);
        }
        if let ControlToBrain::Connection(op) = message {
            self.connection_op(op, Some(link))
                .map_err(|Full| Ended::Lost)?;
            self.screen_holds_changed(link)
                .map_err(|Full| Ended::Lost)?;
            return Ok(None);
        }
        if let ControlToBrain::KeyDown { .. } = message {
            if self.hold_to_talk_enabled() && self.hold_pair_supported(link) {
                self.voice_op(Installer::reload_input, Some(link))
                    .map_err(|Full| Ended::Lost)?;
            }
            return Ok(None);
        }
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        if let Some(probe) = super::audio_fixture::probe() {
            probe.control(&message);
        }
        control_input(message)
    }
}
