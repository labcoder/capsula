//! The key's live owner: each round's sent body, its mapper, its callbacks and its real ending
//! in one place, and a tool loop carried from round to round in memory only
//! (docs/development/anthropic-messages.md, "The tool loop").

use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kumochi_protocol::message::RunId;
use tokio::sync::mpsc;

use super::encode::{self, Conversation, ToolUse};
use super::{Mapper, Reply};
use crate::connections::anthropic::error::Failure;
use crate::connections::anthropic::fence::Fence;
use crate::connections::anthropic::messages::MessagesBody;
use crate::connections::anthropic::request::members;
use crate::connections::anthropic::route::{Offer, Transport, Verified};
use crate::connections::anthropic::sse_map::StopReason;
use crate::connections::model::Events;
use crate::connections::model::budget::Budget;
use crate::connections::model::sse::{Progress, SseEvent};
use crate::context::{ConnectionTag, InputItem};
use crate::coordinator::SendKey;
use crate::engines::{Answer, CallId, EngineFailure, Finish, RunRequest, SendGate, TurnEvent};

/// What a round sends on: the key, the tag it became Ready with, and the run's and the
/// process's request budgets.
pub(crate) struct Sending<'a> {
    pub(crate) transport: &'a Transport,
    pub(crate) authority: &'a Verified,
    pub(crate) tag: ConnectionTag,
    pub(crate) motion: Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Motion {
    First,
    Continue,
    Standalone,
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

/// Messages attempts one run may start: each foreground round, one inline compaction per round
/// and the job its end may start (docs/development/anthropic-messages.md, "One attempt").
pub(crate) const RUN_ATTEMPTS: u32 = 2 * crate::coordinator::MAX_ROUNDS + 1;

/// The newest run's allowance, shared by every `Live` its sends use, so a rebuild that replaces
/// the owner keeps what the run has spent.
#[derive(Debug, Default)]
pub(crate) struct Allowance(Option<(RunId, Arc<Budget>)>);

impl Allowance {
    /// `run`'s allowance: the same one for each of its sends, a fresh one for a later run and
    /// none for an earlier run.
    pub(crate) fn of(&mut self, run: RunId) -> Option<Arc<Budget>> {
        match &self.0 {
            Some((held, budget)) if *held == run => return Some(Arc::clone(budget)),
            Some((held, _)) if *held > run => return None,
            _ => {}
        }
        let budget = Arc::new(Budget::limited(Some(RUN_ATTEMPTS)));
        self.0 = Some((run, Arc::clone(&budget)));
        Some(budget)
    }

    #[cfg(test)]
    pub(crate) fn spent(&self, run: RunId) -> Option<u32> {
        self.0
            .as_ref()
            .filter(|(held, _)| *held == run)
            .map(|(_, budget)| budget.spent())
    }
}

/// Owns each key round from body to ending. Only the next round of the same run takes the
/// continuation, once: a rebuild, a refused send or any other start finds none and refuses
/// locally, so a continuation is never rebuilt.
#[cfg_attr(test, derive(Default))]
pub(crate) struct Live {
    held: Mutex<Option<Held>>,
    fence: Fence,
    budget: Arc<Budget>,
    #[cfg(test)]
    test_pictures: Option<crate::coordinator::PictureRoutes>,
}

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = self.held().as_ref().map(|held| (held.run, held.round));
        f.debug_tuple("Live").field(&held).finish()
    }
}

impl Live {
    /// An owner whose sends charge `budget`, its run's allowance.
    pub(crate) fn new(budget: Arc<Budget>) -> Self {
        Self {
            held: Mutex::default(),
            fence: Fence::default(),
            budget,
            #[cfg(test)]
            test_pictures: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_pictures(
        mut self,
        pictures: crate::coordinator::PictureRoutes,
    ) -> Self {
        self.test_pictures = Some(pictures);
        self
    }

    fn held(&self) -> MutexGuard<'_, Option<Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn retire(&self) {
        let mut held = self.held();
        self.fence.revoke();
        *held = None;
    }

    /// Plays one round of `request` and returns its ending: `ReceiptOrdered` when the send was
    /// refused before any write, so its unsent receipt orders what follows, and `None` when nobody
    /// listens any more. Each read's events go out before
    /// the next read, except items: once the round completed, each call goes out as its stored
    /// `tool_use` item, and the reply stays here for the next round. Signed thinking never leaves.
    pub(crate) async fn round(
        &self,
        request: &RunRequest,
        sending: Sending<'_>,
        events: &mpsc::Sender<TurnEvent>,
        gate: &SendGate,
    ) -> Option<Finish> {
        let failed = |failure| Some(Finish::Failed(EngineFailure::Anthropic(failure)));
        let Ok((body, conversation)) = self.body(request, sending.tag, sending.motion) else {
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
        let read = sending
            .transport
            .messages(
                sending.authority,
                Offer {
                    tag: sending.tag,
                    body: &body,
                    fence: &self.fence,
                    budget: &self.budget,
                },
                ask,
                capture,
                out,
            )
            .await;
        drop(body);
        if matches!(read, Err(Failure::Rebuild)) {
            return Some(Finish::ReceiptOrdered);
        }
        let tool_use = matches!(read, Ok(StopReason::ToolUse));
        let ended = state.ended.take();
        let finish = ended
            .or_else(|| state.mapper.finish(read.map(|_| Progress::Completed)))
            .unwrap_or(Finish::Truncated);
        if state.hand_on(events).await.is_break() {
            return None;
        }
        if finish != Finish::Completed {
            return Some(finish);
        }
        // A tool round ends on `tool_use` with calls, and an answer has neither.
        if tool_use == state.calls.is_empty() {
            return failed(Failure::Protocol);
        }
        if !tool_use {
            return Some(finish);
        }
        let Some((reply, stored)) = state
            .mapper
            .reply()
            .and_then(|reply| stored_calls(&reply).map(|stored| (reply, stored)))
            .filter(|(_, stored)| {
                let stored = stored.iter().map(|(_, call)| call.id());
                stored.eq(state.calls.iter().map(CallId::as_str))
            })
        else {
            return failed(Failure::Protocol);
        };
        if sending.motion == Motion::Standalone {
            return failed(Failure::Protocol);
        }
        let SendKey::Build(key) = request.key else {
            return failed(Failure::Protocol);
        };
        for (index, call) in stored {
            events.send(TurnEvent::KeyCall { index, call }).await.ok()?;
        }
        let mut held = self.held();
        sending
            .authority
            .fence
            .with(|| {
                self.fence.open().then(|| {
                    *held = Some(Held {
                        run: request.run,
                        round: key.round,
                        conversation,
                        reply,
                        calls: state.calls,
                    });
                    finish
                })
            })
            .ok()?
    }

    /// The body to send. A run's first round, a job and a compaction are built from the
    /// request; a later round of a run only continues the round just before it, with that
    /// round's reply and the results the request ends with for its calls, and without one it is
    /// refused.
    fn body(
        &self,
        request: &RunRequest,
        tag: ConnectionTag,
        motion: Motion,
    ) -> Result<(MessagesBody, Conversation), ()> {
        let fresh = || {
            #[cfg(test)]
            if let Some(routes) = self.test_pictures {
                return encode::body_under(&request.request, tag, routes).map_err(drop);
            }
            encode::body(&request.request, tag).map_err(drop)
        };
        if motion == Motion::Standalone {
            return fresh();
        }
        let held = self.held().take();
        if motion == Motion::First {
            return fresh();
        }
        let SendKey::Build(key) = request.key else {
            return Err(());
        };
        let held = held
            .filter(|held| held.run == request.run && held.round + 1 == key.round)
            .ok_or(())?;
        let model = &request.request.choice.as_ref().ok_or(())?.model;
        let results = round_results(&request.request.input, &held.calls).ok_or(())?;
        encode::next(held.conversation, tag, model, held.reply, &results).map_err(drop)
    }
}

/// The reply's calls as the store keeps them, each at its place in the reply.
fn stored_calls(reply: &Reply) -> Option<Vec<(u32, ToolUse)>> {
    let mut calls = Vec::new();
    for (index, block) in (0..).zip(&reply.blocks) {
        let fields = members(block)?;
        let kind = fields.iter().find(|(name, _)| name == "type")?.1;
        if serde_json::from_str::<String>(kind).ok()? == "tool_use" {
            calls.push((index, ToolUse::projected(block)?));
        }
    }
    Some(calls)
}

/// The results of the round that made `calls`, in call order: the items the input ends with,
/// right after that round's own stored calls, one result for each call and nothing else.
fn round_results(input: &[InputItem], calls: &[CallId]) -> Option<Vec<(CallId, String)>> {
    let answers = input
        .iter()
        .rev()
        .take_while(|item| matches!(item, InputItem::FunctionOutput { .. }))
        .count();
    let (before, answers) = input.split_at(input.len() - answers);
    let made = before
        .iter()
        .rev()
        .take_while(|item| matches!(item, InputItem::Native(_)))
        .count();
    let made = before[before.len() - made..]
        .iter()
        .map(|item| match item {
            InputItem::Native(item) => ToolUse::stored(item.bytes()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if answers.len() != calls.len()
        || !made
            .iter()
            .map(ToolUse::id)
            .eq(calls.iter().map(CallId::as_str))
    {
        return None;
    }
    calls
        .iter()
        .map(|call| {
            // As many results as calls, each call answered: no result repeats or answers another.
            answers.iter().find_map(|item| match item {
                InputItem::FunctionOutput {
                    call: answered,
                    output,
                } if answered == call => Some((call.clone(), output.clone())),
                _ => None,
            })
        })
        .collect()
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

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
