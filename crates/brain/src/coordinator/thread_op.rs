//! A task's rename and a thread's model choice: each goes to the store as one write, except a
//! rename of the main thread, whose name is the assistant's setting and whose stored title stays
//! empty.

use kumochi_protocol::message::ThreadOp;

use super::{Coordinator, Effect, StoreOp};
use crate::store::TitleSource;

impl Coordinator {
    pub(super) fn thread_op(&mut self, op: ThreadOp, effects: &mut Vec<Effect>) {
        let op = match op {
            ThreadOp::Rename { thread, .. } if thread == self.main_thread => {
                tracing::warn!(thread = thread.0, "a rename of the main thread was refused");
                return;
            }
            ThreadOp::Rename { thread, title } => StoreOp::SetTitle {
                thread,
                title,
                source: TitleSource::User,
                job: None,
            },
            // No build serves a local server yet, so none is saved as a thread's route.
            ThreadOp::SetModel { thread, choice }
                if choice.connection == kumochi_protocol::message::ConnectionKind::LocalChat =>
            {
                effects.push(Effect::Control(
                    kumochi_protocol::message::ControlToShell::ThreadModelSet {
                        thread,
                        choice,
                        applied: false,
                    },
                ));
                return;
            }
            ThreadOp::SetModel { thread, choice } => {
                let op = StoreOp::SetModel { thread, choice };
                let held = self
                    .ending
                    .iter_mut()
                    .chain(self.current.as_mut())
                    .rfind(|run| run.thread == thread && run.begin.is_some());
                if let Some(run) = held {
                    run.after_begin.push(op);
                    return;
                }
                op
            }
        };
        effects.push(Effect::Store(op));
    }
}
