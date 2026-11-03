use std::cell::Cell;

use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{ControlToBrain, MAX_VISIBLE_SCENES};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{CLOSE_WAIT, Ended};
use crate::coordinator::Input;
use crate::ipc;

pub(super) async fn read_control(
    pipe: &mut (impl AsyncRead + Unpin),
    frames: &mpsc::Sender<(ControlToBrain, usize)>,
    quit: &Cell<Option<Instant>>,
) -> Ended {
    loop {
        match ipc::read_sized::<ControlToBrain>(pipe, Pipe::Control).await {
            Ok(Some((ControlToBrain::Hello(_), _))) => return Ended::Broken("Hello sent twice"),
            Ok(Some((ControlToBrain::Quitting, _))) => {
                let deadline = Instant::now() + CLOSE_WAIT;
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
        ControlToBrain::Pong
        | ControlToBrain::Hello(_)
        | ControlToBrain::Quitting
        | ControlToBrain::Settings(_) => None,
    })
}
