//! The Responses engine: one round per start, on the bound account's bearer, under the send
//! lease, with each read's events handed on before the next read (docs/development/engines.md,
//! "The Responses engine").

use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use kumochi_protocol::Secret;
use kumochi_protocol::message::ErrorKey;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::{Round, body, wipe_images};
use crate::connections::account::Denied;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::model::error::Failure;
use crate::connections::model::sse::{Progress, SseEvent};
#[cfg(not(all(windows, debug_assertions)))]
use crate::connections::model::stream;
use crate::connections::model::{Events, ResponsesBody};
#[cfg(all(windows, debug_assertions))]
use crate::connections::model::{
    stream_observed,
    text_observation::{Observation, Stamp},
};
use crate::coordinator::SendKey;
use crate::engines::{
    self, Answer, Engine, EngineFailure, Finish, RunRequest, SendGate, TurnEvent,
};

/// A bearer, and the generation of the token set it came from.
#[derive(Debug)]
pub(crate) struct Bearer {
    pub(crate) generation: u64,
    pub(crate) token: Secret<Zeroizing<String>>,
}

/// The bearer, or why the account gave none.
pub(crate) type Access = Result<Bearer, Denied>;

/// What a round tells the brain about the account.
pub(crate) enum AccessAsk {
    /// The bearer for a build or job's send, and where the answer goes.
    Bearer {
        key: SendKey,
        answer: oneshot::Sender<Access>,
    },
    /// The provider refused the bearer of this generation.
    Refused(u64),
}

impl fmt::Debug for AccessAsk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { key, .. } => f
                .debug_struct("Bearer")
                .field("key", key)
                .finish_non_exhaustive(),
            Self::Refused(generation) => f.debug_tuple("Refused").field(generation).finish(),
        }
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
        if self.0.send(AccessAsk::Bearer { key, answer }).is_err() {
            return Err(Denied::Unusable);
        }
        answered.await.unwrap_or(Err(Denied::Unusable))
    }

    /// Tells the account the provider refused the bearer of `generation`, so its next access
    /// refreshes it.
    fn refused(&self, generation: u64) {
        self.0.send(AccessAsk::Refused(generation)).ok();
    }
}

/// Answers runs on the signed-in account's plan.
pub(crate) struct Responses<C: Connect>(Arc<Shared<C>>);

struct Shared<C: Connect> {
    client: ModelClient<C>,
    budget: Budget,
    access: AccessHandle,
    #[cfg(all(windows, debug_assertions))]
    observation: Option<Arc<Observation>>,
}

impl<C: Connect> Responses<C> {
    pub(crate) fn new(client: ModelClient<C>, budget: Budget, access: AccessHandle) -> Self {
        Self(Arc::new(Shared {
            client,
            budget,
            access,
            #[cfg(all(windows, debug_assertions))]
            observation: Observation::active().map(Arc::clone),
        }))
    }

    #[cfg(all(test, windows, debug_assertions))]
    pub(super) fn observe_for_tests(mut self, observation: Arc<Observation>) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("the engine has not started")
            .observation = Some(observation);
        self
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
        let Ok(mut value) = body(&request.request) else {
            return failed(Failure::Request);
        };
        let body = ResponsesBody::new(&value);
        wipe_images(&mut value);
        drop(value);
        let body = match body {
            Ok(body) => body,
            Err(failure) => return failed(failure),
        };
        let bearer = match self.access.access(request.key).await {
            Ok(bearer) => bearer,
            Err(Denied::Failed(error)) => {
                return Some(Finish::Failed(EngineFailure::Access(Some(error))));
            }
            Err(Denied::Unusable) => return Some(Finish::Failed(EngineFailure::Access(None))),
        };
        let ask = async {
            match gate.ask().await {
                Answer::Grant(lease) => Some(lease),
                Answer::Rebuild => None,
            }
        };
        let mut round = Round::default();
        #[cfg(all(windows, debug_assertions))]
        let stamp = self
            .observation
            .as_ref()
            .map(|record| Stamp::new(request, record));
        let out = Out {
            round: &mut round,
            events,
            #[cfg(all(windows, debug_assertions))]
            stamp: stamp.clone(),
        };
        #[cfg(not(all(windows, debug_assertions)))]
        let read = stream(&self.client, &body, &bearer.token, ask, &self.budget, out).await;
        #[cfg(all(windows, debug_assertions))]
        let read = stream_observed(
            &self.client,
            &body,
            &bearer.token,
            ask,
            &self.budget,
            out,
            stamp,
        )
        .await;
        if let Err(failure) = read
            && failure.user_error().key() == ErrorKey::AccountRefused
        {
            self.access.refused(bearer.generation);
        }
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
    #[cfg(all(windows, debug_assertions))]
    stamp: Option<Stamp>,
}

impl Events for Out<'_> {
    fn event(&mut self, event: &SseEvent) {
        #[cfg(all(windows, debug_assertions))]
        let previous = self.stamp.as_ref().map(|_| self.round.events.len());
        self.round.event(event);
        #[cfg(all(windows, debug_assertions))]
        if previous.is_some_and(|len| self.round.events.len() > len)
            && matches!(self.round.events.last(), Some(TurnEvent::TextDelta(delta)) if !delta.is_empty())
            && let Some(stamp) = self.stamp.take()
        {
            stamp.first_text();
        }
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
