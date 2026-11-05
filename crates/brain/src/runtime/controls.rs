use std::cell::Cell;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{ControlToBrain, MAX_VISIBLE_SCENES};
use kumochi_protocol::tap::{Direction, Tapped};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Brain, Ended, Full, Link, close_wait, settings};
use crate::connections::account::Input as AccountInput;
use crate::coordinator::Input;
use crate::ipc;
use crate::tap::Tap;

pub(super) async fn read_control(
    pipe: &mut (impl AsyncRead + Unpin),
    frames: &mpsc::Sender<(ControlToBrain, usize)>,
    quit: &Cell<Option<Instant>>,
) -> Ended {
    loop {
        match ipc::read_sized::<ControlToBrain>(pipe, Pipe::Control).await {
            Ok(Some((ControlToBrain::Hello(_), _))) => return Ended::Broken("Hello sent twice"),
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

pub(super) fn control_input(message: ControlToBrain) -> Result<Option<Input>, Ended> {
    Ok(match message {
        ControlToBrain::Stop { run, revealed } => Some(Input::Stop { run, revealed }),
        ControlToBrain::ClearDrawing { scene_ids } if scene_ids.len() > MAX_VISIBLE_SCENES => {
            return Err(Ended::Broken("more scenes cleared than can be visible"));
        }
        ControlToBrain::ClearDrawing { scene_ids } => Some(Input::Clear { scenes: scene_ids }),
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
            cut,
            ..
        } => Some(Input::Drawing(crate::drawing::Input::StepFailed {
            run,
            scene,
            index,
            cut,
        })),
        // shortcut: B2a admits holds and consumes focus hints and exclusion lease changes.
        message @ (ControlToBrain::HoldRecognized { .. }
        | ControlToBrain::KeyUp { .. }
        | ControlToBrain::GestureCancel { .. }
        | ControlToBrain::FocusHint { .. }
        | ControlToBrain::ExclusionLapsed { .. }) => {
            tracing::debug!(op = message.op(), "hold control ignored");
            None
        }
        message @ (ControlToBrain::GitHub(_) | ControlToBrain::Issue(_)) => {
            tracing::debug!(op = message.op(), "github or issue control ignored");
            None
        }
        // shortcut: nothing answers Connection or Warm until A1, C2 and C5b build them.
        ControlToBrain::Pong
        | ControlToBrain::Hello(_)
        | ControlToBrain::Quitting
        | ControlToBrain::Settings(_)
        | ControlToBrain::AccountOp(_)
        | ControlToBrain::Connection(_)
        | ControlToBrain::Warm { .. } => None,
    })
}

impl Brain {
    /// Applies a control frame with no shell to tell, as `serve` does once the core has gone.
    pub(super) fn control_offline(&mut self, message: ControlToBrain, len: usize, tap: &mut Tap) {
        tap.record(Pipe::Control, Direction::ToBrain, &message, len);
        if let ControlToBrain::AccountOp(op) = message {
            let applied = self.account_input(AccountInput::Op(op), None);
            debug_assert!(applied.is_ok(), "applying without a shell can't fail");
            return;
        }
        if let ControlToBrain::Settings(op) = message {
            // Still written; the shell that sent it is gone, so nothing answers it.
            self.settings_ops.push_back(settings::Queued {
                op,
                epoch: self.epoch,
            });
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
        if message == ControlToBrain::Pong {
            *ping_sent = None;
        }
        if let ControlToBrain::Settings(op) = message {
            self.settings_ops.push_back(settings::Queued {
                op,
                epoch: self.epoch,
            });
            self.pump_settings(Some(link)).map_err(|Full| Ended::Lost)?;
            return Ok(None);
        }
        if let ControlToBrain::AccountOp(op) = message {
            self.account_input(AccountInput::Op(op), Some(link))
                .map_err(|Full| Ended::Lost)?;
            return Ok(None);
        }
        control_input(message)
    }
}
