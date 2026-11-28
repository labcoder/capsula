//! The local engine: one Chat Completions round per start to the model server on this computer,
//! carrying only the app's instructions and a local task's plain text (docs/development/engines.md,
//! "The local engine"). Nothing builds it while [`LOCAL_CHAT`] is off.
//!
//! [`LOCAL_CHAT`]: kumochi_protocol::constants::LOCAL_CHAT

use std::fmt;
use std::ops::ControlFlow;
use std::time::Duration;

use kumochi_protocol::message::{ConnectionKind, ModelSlug};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{Answer, Engine, EngineFailure, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::config::tuning::ModelTuning;
use crate::connections::local::endpoint::LoopbackEndpoint;
use crate::connections::local::request::{ChatBody, ChatMessage, Role as Wire};
use crate::connections::local::{self, Failure, Sink};
use crate::connections::model::budget::Budget;
use crate::context::{InputItem, ModelRequest, Role};

/// Answers a local task's runs on one endpoint and model, which never change.
pub(crate) struct LocalEngine {
    endpoint: LoopbackEndpoint,
    model: ModelSlug,
    budget: Budget,
}

impl LocalEngine {
    pub(crate) fn new(endpoint: LoopbackEndpoint, model: ModelSlug, budget: Budget) -> Self {
        Self {
            endpoint,
            model,
            budget,
        }
    }
}

impl fmt::Debug for LocalEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalEngine")
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl Engine for LocalEngine {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let (endpoint, model, budget) = (self.endpoint, self.model.clone(), self.budget.clone());
        Box::pin(async move {
            let mut pending = String::new();
            let finish = round(
                endpoint,
                &model,
                &budget,
                &request.request,
                &events,
                gate,
                &mut pending,
            )
            .await;
            deliver(
                &events,
                pending,
                finish,
                ModelTuning::default().backpressure,
            )
            .await;
        })
    }
}

/// The pinned request's body, then the ask inside the send, then the stream. A Rebuild was
/// never written, so the coordinator builds it again.
async fn round(
    endpoint: LoopbackEndpoint,
    model: &ModelSlug,
    budget: &Budget,
    request: &ModelRequest,
    events: &mpsc::Sender<TurnEvent>,
    gate: SendGate,
    pending: &mut String,
) -> Finish {
    let failed = |failure| Finish::Failed(EngineFailure::Local(failure));
    let pinned = request.choice.as_ref().is_some_and(|choice| {
        choice.connection == ConnectionKind::LocalChat && choice.model == *model
    });
    if !pinned {
        return failed(Failure::Request);
    }
    let body = match encode(model, request) {
        Ok(body) => body,
        Err(failure) => return failed(failure),
    };
    let ask = async {
        match gate.ask().await {
            Answer::Grant(lease) => Some(lease),
            Answer::Rebuild => None,
        }
    };
    let out = Pending {
        text: pending,
        events,
    };
    match local::stream(endpoint, &body, ask, budget, out).await {
        Ok(()) => Finish::Completed,
        Err(Failure::Rebuild) => Finish::ReceiptOrdered,
        Err(failure) => failed(failure),
    }
}

/// The app's instructions, then the task's turns and answers. Anything else, a tool offer
/// included, refuses the request before any connection.
fn encode(model: &ModelSlug, request: &ModelRequest) -> Result<ChatBody, Failure> {
    if !request.tools.is_empty() {
        return Err(Failure::Request);
    }
    let mut messages = vec![ChatMessage {
        role: Wire::System,
        text: &request.instructions,
    }];
    for item in &request.input {
        let InputItem::Message { role, text } = item else {
            return Err(Failure::Request);
        };
        let role = match role {
            Role::User => Wire::User,
            Role::Assistant => Wire::Assistant,
            Role::Developer => return Err(Failure::Request),
        };
        messages.push(ChatMessage { role, text });
    }
    ChatBody::new(model, &messages)
}

/// Text the last read left, then the round's outcome, all within one `wait` for room. Text
/// before a failure arrives before it; nothing arrives late.
async fn deliver(
    events: &mpsc::Sender<TurnEvent>,
    pending: String,
    finish: Finish,
    wait: Duration,
) {
    // A round whose outcome can't be told is reported by its pump as unfinished.
    timeout(wait, async {
        if !pending.is_empty() {
            events.send(TurnEvent::TextDelta(pending)).await?;
        }
        events.send(TurnEvent::Finished(finish)).await
    })
    .await
    .ok();
}

/// A read's text, handed on only once the round's channel has room for it.
struct Pending<'a> {
    text: &'a mut String,
    events: &'a mpsc::Sender<TurnEvent>,
}

impl Sink for Pending<'_> {
    fn text(&mut self, text: &str) {
        self.text.push_str(text);
    }

    async fn taken(&mut self) -> ControlFlow<()> {
        let Ok(room) = self.events.reserve().await else {
            return ControlFlow::Break(());
        };
        room.send(TurnEvent::TextDelta(std::mem::take(self.text)));
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests;
