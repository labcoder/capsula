//! The key's live owner: each round's sent body, its mapper, its callbacks and its real ending
//! in one place, and a tool loop carried from round to round in memory only
//! (docs/development/anthropic-messages.md, "The tool loop").

use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Mutex, MutexGuard, PoisonError};

use kumochi_protocol::Secret;
use kumochi_protocol::message::RunId;
use serde_json::json;
use tokio::sync::mpsc;

use super::encode::{self, Conversation};
use super::{Mapper, Reply};
use crate::connections::anthropic::error::Failure;
use crate::connections::anthropic::key::AnthropicKey;
use crate::connections::anthropic::messages::{MessagesBody, stream};
use crate::connections::anthropic::request::literal;
use crate::connections::anthropic::tls::AnthropicConnect;
use crate::connections::model::Events;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::ModelClient;
use crate::connections::model::sse::{Progress, SseEvent};
use crate::context::{ConnectionTag, InputItem};
use crate::coordinator::SendKey;
use crate::engines::{
    Answer, CallId, EngineFailure, Finish, Opaque, RunRequest, SendGate, TurnEvent,
};

/// What a round sends on: the key, the tag it became Ready with, and the run's and the
/// process's request budgets.
pub(crate) struct Sending<'a> {
    pub(crate) key: &'a Secret<AnthropicKey>,
    pub(crate) tag: ConnectionTag,
    pub(crate) run: &'a Budget,
    pub(crate) live: &'a Budget,
}

/// A run's tool loop between two rounds: the request last sent, the reply that answered it and
/// the calls that reply made.
struct Held {
    run: RunId,
    round: u32,
    conversation: Conversation,
    reply: Reply,
    calls: Vec<CallId>,
}

/// Owns each key round from body to ending. Only the next round of the same run takes the
/// continuation, once: a rebuild, a refused send or any other start finds none and refuses
/// locally, so a continuation is never rebuilt.
#[derive(Default)]
pub(crate) struct Live(Mutex<Option<Held>>);

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = self.held().as_ref().map(|held| (held.run, held.round));
        f.debug_tuple("Live").field(&held).finish()
    }
}

impl Live {
    fn held(&self) -> MutexGuard<'_, Option<Held>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Plays one round of `request` and returns its ending; `None` when the coordinator said
    /// rebuild, so nothing was sent, or nobody listens any more. Each read's events go out before
    /// the next read, except items: once the round completed, each call goes out as its stored
    /// `tool_use` item, and the reply stays here for the next round. Signed thinking never leaves.
    pub(crate) async fn round<C: AnthropicConnect>(
        &self,
        client: &ModelClient<C>,
        request: &RunRequest,
        sending: Sending<'_>,
        events: &mpsc::Sender<TurnEvent>,
        gate: &SendGate,
    ) -> Option<Finish> {
        let failed = |failure| Some(Finish::Failed(EngineFailure::Anthropic(failure)));
        let Ok((body, conversation)) = self.body(request, sending.tag) else {
            return failed(Failure::Request);
        };
        let mut state = State {
            mapper: Mapper::answering(&conversation),
            calls: Vec::new(),
            ended: None,
        };
        let capture = state.mapper.capture();
        let ask = async {
            match gate.ask().await {
                Answer::Grant(lease) => Some(lease),
                Answer::Rebuild => None,
            }
        };
        let out = Out {
            state: &mut state,
            events,
        };
        let read = stream(
            client,
            &body,
            sending.key,
            ask,
            sending.run,
            sending.live,
            capture,
            out,
        )
        .await;
        drop(body);
        if matches!(read, Err(Failure::Rebuild)) {
            return None;
        }
        let ended = state.ended.take();
        let finish = ended
            .or_else(|| state.mapper.finish(read.map(|_| Progress::Completed)))
            .unwrap_or(Finish::Truncated);
        if state.hand_on(events).await.is_break() {
            return None;
        }
        if finish != Finish::Completed || state.calls.is_empty() {
            return Some(finish);
        }
        let Some((reply, stored)) = state
            .mapper
            .reply()
            .and_then(|reply| stored_calls(&reply).map(|stored| (reply, stored)))
        else {
            return failed(Failure::Protocol);
        };
        for (index, item) in stored {
            events
                .send(TurnEvent::OutputItem { index, item })
                .await
                .ok()?;
        }
        if let SendKey::Build(key) = request.key {
            *self.held() = Some(Held {
                run: request.run,
                round: key.round,
                conversation,
                reply,
                calls: state.calls,
            });
        }
        Some(finish)
    }

    /// The body to send. A run's first round, a job and a compaction are built from the
    /// request; a later round of a run only continues the round just before it, with that
    /// round's reply and the request's results for its calls, and without one it is refused.
    fn body(
        &self,
        request: &RunRequest,
        tag: ConnectionTag,
    ) -> Result<(MessagesBody, Conversation), ()> {
        let fresh = || encode::body(&request.request, tag).map_err(drop);
        // A job and a compaction offer no tools, so neither is a tool round.
        let SendKey::Build(key) = request.key else {
            return fresh();
        };
        if request.request.tools.is_empty() {
            return fresh();
        }
        let held = self.held().take();
        if key.round == 1 {
            return fresh();
        }
        let held = held
            .filter(|held| held.run == request.run && held.round + 1 == key.round)
            .ok_or(())?;
        let model = &request.request.choice.as_ref().ok_or(())?.model;
        let results = held
            .calls
            .iter()
            .map(|call| {
                request.request.input.iter().find_map(|item| match item {
                    InputItem::FunctionOutput {
                        call: answered,
                        output,
                    } if answered == call => Some((call.clone(), output.clone())),
                    _ => None,
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(())?;
        encode::next(held.conversation, tag, model, held.reply, &results).map_err(drop)
    }
}

/// The reply's calls as the store keeps them: each `tool_use` block's id, name and input and
/// nothing else, at its place in the reply.
fn stored_calls(reply: &Reply) -> Option<Vec<(u32, Opaque)>> {
    let mut items = Vec::new();
    for (index, block) in (0..).zip(&reply.blocks) {
        let block = literal(block.as_bytes()).ok()?;
        if block["type"] != "tool_use" {
            continue;
        }
        let item = json!({
            "type": "tool_use",
            "id": block["id"],
            "name": block["name"],
            "input": block["input"],
        });
        items.push((index, Opaque::new(serde_json::to_vec(&item).ok()?).ok()?));
    }
    Some(items)
}

/// One round's mapper, the calls it made ready, and an ending a read already gave.
struct State {
    mapper: Mapper,
    calls: Vec<CallId>,
    ended: Option<Finish>,
}

impl State {
    /// Hands the mapper's events on, keeping its items back.
    async fn hand_on(&mut self, events: &mpsc::Sender<TurnEvent>) -> ControlFlow<()> {
        for event in self.mapper.take() {
            match &event {
                TurnEvent::OutputItem { .. } => continue,
                TurnEvent::ToolCallReady { call, .. } => self.calls.push(call.clone()),
                _ => {}
            }
            if events.send(event).await.is_err() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

/// A round's callbacks: each read's events go to the pump before the next read, and a gone
/// pump or a broken round stops the stream.
struct Out<'a> {
    state: &'a mut State,
    events: &'a mpsc::Sender<TurnEvent>,
}

impl Events for Out<'_> {
    fn event(&mut self, event: &SseEvent) {
        self.state.mapper.event(event);
    }

    async fn taken(&mut self) -> ControlFlow<()> {
        self.state.hand_on(self.events).await?;
        if let Some(finish) = self.state.mapper.finish(Ok(Progress::More)) {
            self.state.ended = Some(finish);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}
