//! A coordinator-admitted Anthropic round on a verified authority.

use std::{fmt, sync::Arc};

use tokio::sync::mpsc;

use crate::connections::anthropic::route::{Transport, Verified};
use crate::context::ConnectionTag;
use crate::engines::anthropic::live::{Live, Motion, Sending};
use crate::engines::{Engine, Round, RunRequest, SendGate, TurnEvent};

pub(super) struct Question {
    pub(super) transport: Transport,
    pub(super) authority: Arc<Verified>,
    pub(super) live: Arc<Live>,
    pub(super) tag: ConnectionTag,
    pub(super) motion: Motion,
}

impl fmt::Debug for Question {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnthropicKeyQuestion")
    }
}

impl Engine for Question {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let transport = self.transport.clone();
        let authority = Arc::clone(&self.authority);
        let live = Arc::clone(&self.live);
        let (tag, motion) = (self.tag, self.motion);
        Box::pin(async move {
            let sending = Sending {
                transport: &transport,
                authority: &authority,
                tag,
                motion,
            };
            if let Some(finish) = live.round(&request, sending, &events, &gate).await {
                events.send(TurnEvent::Finished(finish)).await.ok();
            }
        })
    }
}
