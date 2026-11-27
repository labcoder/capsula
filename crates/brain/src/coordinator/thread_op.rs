//! A task's rename and a thread's model choice: each goes to the store as one write, except a
//! rename of the main thread, whose name is the assistant's setting and whose stored title stays
//! empty. The main thread's model choice is written like a task's.

use kumochi_protocol::message::ThreadOp;

use super::{Coordinator, Effect, StoreOp};
use crate::store::TitleSource;

impl Coordinator {
    pub(super) fn thread_op(&self, op: ThreadOp, effects: &mut Vec<Effect>) {
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
            ThreadOp::SetModel {
                request,
                thread,
                choice,
            } => {
                self.set_model(request, thread, choice, effects);
                return;
            }
        };
        effects.push(Effect::Store(op));
    }
}
