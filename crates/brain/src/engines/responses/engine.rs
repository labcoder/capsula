//! The Responses engine: one round per start, on the bound account's bearer, under the send
//! lease, with each read's events handed on before the next read (docs/development/engines.md,
//! "The Responses engine").

use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use kumochi_protocol::Secret;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::{Round, body};
use crate::connections::account::Denied;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::model::error::Failure;
use crate::connections::model::sse::{Progress, SseEvent};
use crate::connections::model::{Events, ResponsesBody, stream};
use crate::coordinator::SendKey;
use crate::engines::{
    self, Answer, Engine, EngineFailure, Finish, RunRequest, SendGate, TurnEvent,
};

/// The bearer, or why the account gave none.
pub(crate) type Access = Result<Secret<Zeroizing<String>>, Denied>;

/// A round's ask for the bearer: which build or job sends, and where the answer goes.
pub(crate) struct AccessAsk {
    pub(crate) key: SendKey,
    pub(crate) answer: oneshot::Sender<Access>,
}

impl fmt::Debug for AccessAsk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessAsk")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// How a round asks the brain for the bearer of the connection its run is bound to.
#[derive(Clone, Debug)]
pub(crate) struct AccessHandle(mpsc::UnboundedSender<AccessAsk>);

impl AccessHandle {
    /// A handle, and the asks it sends, for the brain to answer.
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<AccessAsk>) {
        let (asks, asked) = mpsc::unbounded_channel();
        (Self(asks), asked)
    }

    /// The bearer for `key`'s send; a brain that's gone gives none.
    async fn access(&self, key: SendKey) -> Access {
        let (answer, answered) = oneshot::channel();
        if self.0.send(AccessAsk { key, answer }).is_err() {
            return Err(Denied::Unusable);
        }
        answered.await.unwrap_or(Err(Denied::Unusable))
    }
}

/// Answers runs on the signed-in account's plan.
pub(crate) struct Responses<C: Connect>(Arc<Shared<C>>);

struct Shared<C: Connect> {
    client: ModelClient<C>,
    budget: Budget,
    access: AccessHandle,
}

impl<C: Connect> Responses<C> {
    pub(crate) fn new(client: ModelClient<C>, budget: Budget, access: AccessHandle) -> Self {
        Self(Arc::new(Shared {
            client,
            budget,
            access,
        }))
    }
}

impl<C: Connect> fmt::Debug for Responses<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Responses")
            .field("budget", &self.0.budget)
            .finish_non_exhaustive()
    }
}

impl<C: Connect + 'static> Engine for Responses<C> {
    fn start(
        &self,
        request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        gate: SendGate,
    ) -> engines::Round {
        let shared = Arc::clone(&self.0);
        Box::pin(async move {
            if let Some(finish) = shared.round(&request, &events, gate).await {
                events.send(TurnEvent::Finished(finish)).await.ok();
            }
        })
    }
}

impl<C: Connect> Shared<C> {
    /// The checked body, then the bearer, then the ask inside the send, then the stream. A
    /// Rebuild ends the round with nothing more, since the coordinator already dropped it.
    async fn round(
        &self,
        request: &RunRequest,
        events: &mpsc::Sender<TurnEvent>,
        gate: SendGate,
    ) -> Option<Finish> {
        let failed = |failure| Some(Finish::Failed(EngineFailure::Stream(failure)));
        let Ok(value) = body(&request.request) else {
            return failed(Failure::Request);
        };
        let body = match ResponsesBody::new(&value) {
            Ok(body) => body,
            Err(failure) => return failed(failure),
        };
        let Ok(bearer) = self.access.access(request.key).await else {
            return Some(Finish::Failed(EngineFailure::Access));
        };
        let ask = async {
            match gate.ask().await {
                Answer::Grant(lease) => Some(lease),
                Answer::Rebuild => None,
            }
        };
        let mut round = Round::default();
        let out = Out {
            round: &mut round,
            events,
        };
        let read = stream(&self.client, &body, &bearer, ask, &self.budget, out).await;
        drop(bearer);
        if read == Err(Failure::Rebuild) {
            return None;
        }
        for event in round.take() {
            events.send(event).await.ok()?;
        }
        match round.end(read.map(|()| Progress::Completed)) {
            Some(TurnEvent::Finished(finish)) => Some(finish),
            _ => Some(Finish::Truncated),
        }
    }
}

/// A round's events on their way out: each read's go to the pump before the next read, and a
/// gone pump or a broken round stops the stream.
struct Out<'a> {
    round: &'a mut Round,
    events: &'a mpsc::Sender<TurnEvent>,
}

impl Events for Out<'_> {
    fn event(&mut self, event: &SseEvent) {
        self.round.event(event);
    }

    async fn taken(&mut self) -> ControlFlow<()> {
        for event in self.round.take() {
            if self.events.send(event).await.is_err() {
                return ControlFlow::Break(());
            }
        }
        if self.round.end(Ok(Progress::More)).is_some() {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}
