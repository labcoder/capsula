//! Claude Code's calls into Kumochi's tools. A launch's listener hands each admitted call over;
//! the coordinator dispatches it on the brain's loop under the build and launch it authorized,
//! and the call's one reply goes back once it settles: at once for a refusal, on its write's
//! acknowledgement otherwise. The listener admitted the call before the mailbox, so the core
//! checks the launch's authority again (`Coordinator::mcp_authorized`) where the call reaches it
//! and where its reply would leave: a call whose launch lost it meanwhile is refused unrun, and a
//! reply for one is let go, which the listener tells the model as an ended call.

use tokio::sync::oneshot;

use super::super::{Brain, Full, Link};
use crate::bridge::listener::{McpCall, McpOrigin, McpReply};
use crate::dispatch::{Dispatch, MAX_ARGS};
use crate::engines::claude_code::launch::Bind;

/// Stands in for arguments the listener found over [`MAX_ARGS`] and kept out of the mailbox: the
/// dispatcher reads only their length, so the call gets the cap's usual refusal.
static OVER_CAP: [u8; MAX_ARGS + 1] = [0; MAX_ARGS + 1];

/// A dispatched call that waits on its settlement.
#[derive(Debug)]
pub(super) struct Waiter {
    origin: McpOrigin,
    reply: oneshot::Sender<McpReply>,
}

impl Brain {
    /// Authorizes a live launch of this connection's generation, not yet bound, for its build's
    /// calls while the build is the current admitted one for the launch's thread and offers every
    /// tool the launch does; the core holds it to its gate and effective deadline from then on.
    pub(super) fn mcp_bind(&mut self, bind: Bind) {
        let generation = self.claude.generation;
        let held = self.claude.launches.iter_mut().find(|held| {
            held.id == bind.launch
                && held.generation == generation
                && !held.replaced
                && !held.bound
                && !held.control.is_revoked()
                && !held.control.is_ended()
        });
        let until = held.and_then(|held| {
            let gate = held.control.gate().clone();
            let until = self.coordinator.mcp_bound(&bind, held.thread, gate);
            held.bound = until.is_some();
            until
        });
        bind.reply.send(until).ok();
    }

    /// Hands an admitted call to the coordinator, answering it at once when it settled there. A
    /// call nobody waits for any more starts nothing, and the core refuses one whose launch lost
    /// its authority before anything is charged or run.
    pub(super) fn mcp_call(
        &mut self,
        call: McpCall,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let McpCall {
            origin,
            tool,
            args,
            reply,
        } = call;
        if reply.is_closed() {
            return Ok(());
        }
        let mut effects = Vec::new();
        let dispatch = self.coordinator.mcp_call(
            origin.build,
            origin.launch,
            origin.key.clone(),
            (tool, args.as_deref().unwrap_or(&OVER_CAP)),
            &mut effects,
        );
        match dispatch {
            Dispatch::Answered(output) => {
                reply
                    .send(self.coordinator.mcp_reply(&origin, &output))
                    .ok();
            }
            Dispatch::Joined | Dispatch::Committing => {
                self.claude.waiting.push(Waiter { origin, reply });
            }
        }
        self.settle_mcp();
        self.apply(effects, link).map_err(|_| Full)
    }

    /// Answers every waiting call that settled and lets go of every one no longer waited for or
    /// whose launch lost its authority. It runs after each coordinator transition.
    pub(in crate::runtime) fn settle_mcp(&mut self) {
        let mut at = 0;
        while let Some(waiter) = self.claude.waiting.get(at) {
            let McpOrigin { build, launch, key } = &waiter.origin;
            let answer = if waiter.reply.is_closed() {
                Err(crate::coordinator::Stale)
            } else {
                self.coordinator.take_mcp_answer(*build, *launch, key)
            };
            if let Ok(None) = answer {
                at += 1;
                continue;
            }
            let waiter = self.claude.waiting.swap_remove(at);
            if let Ok(Some(output)) = answer {
                let reply = self.coordinator.mcp_reply(&waiter.origin, &output);
                waiter.reply.send(reply).ok();
            }
        }
    }
}
