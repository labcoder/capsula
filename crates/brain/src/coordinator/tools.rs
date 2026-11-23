//! The tool loop: a run's calls and native items, one model round at a time. A round that made
//! calls is stored with a result for each, and the run's next round is built from the store; a
//! round that made none is stored and ends the run. Budgets are charged at admission (D13-M2):
//! a call before anything of it is kept, a round before its build.

use std::fmt;

use kumochi_protocol::message::{ConnectionKind, ModelSlug, RunId, ThreadId};

use super::{Coordinator, Effect, ReplyText, Stale, StoreOp};
use crate::context::{BOUND, Route};
use crate::dispatch::{Caller, Dispatch, ToolOutput};
use crate::engines::anthropic::encode::ToolUse;
use crate::engines::{CallId, Opaque, ToolName};
use crate::store::Failure;

/// The most model rounds one run may have [guess, from §13b]; `[tuning.runs]` may only lower it.
pub const MAX_ROUNDS: u32 = 8;
/// The most tool calls one run may make, across its rounds [guess, from §13b]; `[tuning.runs]` may
/// only lower it.
pub const MAX_CALLS: u32 = 16;

impl super::Run {
    pub(super) fn pictured_key_model(&self) -> Option<&ModelSlug> {
        self.picture_recipient
            .as_ref()
            .filter(|recipient| recipient.route.kind == ConnectionKind::AnthropicKey)
            .map(|recipient| &recipient.model)
    }
}
/// One round's write, in one transaction: the text streamed since the last write, the round's
/// native items in output order, then each call's answer in call order. `RoundStored`
/// answers it.
#[derive(Clone, Debug, PartialEq)]
pub struct RoundItems {
    pub(crate) thread: ThreadId,
    pub(crate) run: RunId,
    pub(crate) round: u32,
    /// The run's route, which replays the items.
    pub(crate) connection: Route,
    pub(crate) text: Option<ReplyText>,
    pub(crate) items: Vec<Opaque>,
    /// The Anthropic key's calls, in output order: the only items its rounds store.
    pub(crate) key_calls: Vec<ToolUse>,
    pub(crate) pictured_key_model: Option<ModelSlug>,
    pub(crate) calls: Vec<(CallId, ToolOutput)>,
}

/// One of the round's calls: its tool, its arguments once ready, and its answer once it has one.
pub(super) struct Open {
    pub(super) call: CallId,
    name: ToolName,
    args: Option<String>,
    output: Option<ToolOutput>,
    pub(super) provider: Option<super::provider_draw::ProviderDraw>,
}

/// A run's tool loop.
#[derive(Default)]
pub(super) struct Loop {
    /// Calls accepted across the run's rounds.
    calls: u32,
    /// This round's calls, in call order.
    pub(super) open: Vec<Open>,
    /// This round's calls went to the dispatcher; the round is stored once each is answered.
    dispatched: bool,
    /// This round's native items, with their output index.
    items: Vec<(u32, Opaque)>,
    key_calls: Vec<(u32, ToolUse)>,
    pub(super) key_results: Vec<(CallId, ToolOutput)>,
    item_bytes: usize,
    /// The round being stored, and whether it's the run's last.
    writing: Option<(u32, bool)>,
}

impl fmt::Debug for Loop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loop")
            .field("calls", &self.calls)
            .field("open", &self.open.len())
            .field("dispatched", &self.dispatched)
            .field("items", &self.items.len())
            .field("key_calls", &self.key_calls.len())
            .field("key_results", &self.key_results.len())
            .field("item_bytes", &self.item_bytes)
            .field("writing", &self.writing)
            .finish()
    }
}

impl Loop {
    /// Charges one call; false when the run has made all `limit` allows.
    pub(super) fn charge(&mut self, limit: u32) -> bool {
        if self.calls >= limit {
            return false;
        }
        self.calls += 1;
        true
    }

    /// A call of the round; false when the round already has it.
    pub(super) fn open(&mut self, call: CallId, name: ToolName) -> bool {
        if self.open.iter().any(|open| open.call == call) {
            return false;
        }
        self.open.push(Open {
            call,
            name,
            args: None,
            output: None,
            provider: None,
        });
        true
    }

    /// The call is open and its arguments aren't ready yet.
    pub(super) fn takes(&self, call: &CallId) -> bool {
        self.open
            .iter()
            .any(|open| open.call == *call && open.args.is_none())
    }

    pub(super) fn ready(&mut self, call: &CallId, args: String) {
        if let Some(open) = self.open.iter_mut().find(|open| open.call == *call) {
            open.args = Some(args);
        }
    }

    pub(super) fn all_ready(&self) -> bool {
        self.open.iter().all(|open| open.args.is_some())
    }

    pub(super) fn dispatched(&self) -> bool {
        self.dispatched
    }

    pub(super) fn waiting(&self, call: &str) -> bool {
        self.dispatched
            && self
                .open
                .iter()
                .any(|open| open.call.as_str() == call && open.output.is_none())
    }

    /// Forgets the round a voided build produced: its calls, answered or not, and its items.
    pub(super) fn drop_round(&mut self) {
        self.open.clear();
        self.dispatched = false;
        self.items.clear();
        self.key_calls.clear();
        self.item_bytes = 0;
    }

    /// Answers a dispatched call still waiting; false when none is.
    pub(super) fn answer(&mut self, call: &CallId, output: ToolOutput) -> bool {
        let dispatched = self.dispatched;
        let waiting = self
            .open
            .iter_mut()
            .find(|open| open.call == *call && open.output.is_none());
        let Some(open) = waiting.filter(|open| {
            dispatched
                || open
                    .provider
                    .as_ref()
                    .is_some_and(|provider| provider.registered)
        }) else {
            return false;
        };
        open.output = Some(output);
        true
    }

    pub(super) fn provider_registered(&self, call: &CallId, key: Option<super::BuildKey>) -> bool {
        self.open
            .iter()
            .find(|open| open.call == *call)
            .and_then(|open| open.provider.as_ref())
            .is_some_and(|provider| provider.registered && Some(provider.build) == key)
    }

    /// The round made calls the loop still has to answer.
    pub(super) fn calling(&self) -> bool {
        !self.open.is_empty()
    }

    /// Keeps a native item; false when the round's items are over a request's bound.
    pub(super) fn keep(&mut self, index: u32, item: Opaque) -> bool {
        if self.item_bytes + item.size() > BOUND {
            return false;
        }
        self.item_bytes += item.size();
        self.items.push((index, item));
        true
    }

    /// Keeps a call of the key's reply, within the same bound as items.
    pub(super) fn keep_call(&mut self, index: u32, call: ToolUse) -> bool {
        let size = call.json().len();
        if self.item_bytes + size > BOUND {
            return false;
        }
        self.item_bytes += size;
        self.key_calls.push((index, call));
        true
    }

    pub(super) fn writing(&self) -> bool {
        self.writing.is_some()
    }

    /// The pending round write failed; true when there was one.
    pub(super) fn write_failed(&mut self) -> bool {
        self.writing.take().is_some()
    }
}

impl super::Run {
    /// The admitted build's model call: on the key route its identity carries the logical round,
    /// so an id a later round reuses is a new call; every other route keeps one identity per run.
    pub(super) fn model_caller(&self, call: CallId) -> Caller {
        let run = self.id;
        if self
            .route
            .is_some_and(|route| route.kind == ConnectionKind::AnthropicKey)
        {
            let round = self.build.as_ref().map_or(1, |build| build.key().round);
            Caller::ModelRound { run, round, call }
        } else {
            Caller::Model { run, call }
        }
    }
}

impl Coordinator {
    /// The current run's round completed and its steps are stored: a round with calls is stored
    /// with their results unless it was the run's last allowed round, a round with only native
    /// items is stored before the run ends, and a round with neither ends it.
    pub(super) fn round_end(&mut self, run: RunId, effects: &mut Vec<Effect>) -> Result<(), Stale> {
        let rounds = self.tuning.rounds;
        let current = self.current(run)?;
        let round = current.build.as_ref().map_or(1, |build| build.key().round);
        let calling = current.tools.calling();
        if calling && round >= rounds {
            return self.fail(run, Failure::Budget, effects);
        }
        if !calling && current.tools.items.is_empty() {
            return self.complete(run, effects);
        }
        if current.route.is_none() {
            return self.fail(run, Failure::Engine, effects);
        }
        if calling && !current.tools.dispatched {
            current.tools.dispatched = true;
            // The engine is done with the round, so its calls run; a call never sees it finishing.
            current.finishing = false;
            let calls: Vec<_> = current
                .tools
                .open
                .iter()
                .filter(|open| open.provider.is_none())
                .map(|open| {
                    let caller = current.model_caller(open.call.clone());
                    (caller, open.call.clone(), open.name, open.args.clone())
                })
                .collect();
            for (caller, call, name, args) in calls {
                let args = args.unwrap_or_default();
                let dispatch = self.tool_call(caller, name, args.as_bytes(), effects);
                let current = self.current(run)?;
                // A delete reached what the build rendered: the round is built again, so the
                // response's calls, items and text are dropped, not stored.
                // shortcut: steps a voided response drew stay, and its text stays on screen, until
                // the round-retraction follow-up teaches the shell to retract a round.
                if !current
                    .build
                    .as_ref()
                    .is_some_and(super::admission::Build::admitted)
                {
                    current.finishing = false;
                    return Ok(());
                }
                if let Dispatch::Answered(output) = dispatch {
                    current.tools.answer(&call, output);
                }
            }
        }
        self.store_round(run, effects)
    }

    /// Stores the current round once each of its calls is answered.
    pub(super) fn store_round(
        &mut self,
        run: RunId,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let pending = self.pending_deletes();
        let marks = self.screen_marks == crate::drawing::ScreenMarks::On;
        let current = self.current(run)?;
        let screen = marks && current.pictured;
        if (current.tools.calling() && !current.tools.dispatched)
            || current.tools.open.iter().any(|open| open.output.is_none())
        {
            return Ok(());
        }
        // A delete accepted while the calls were written reached what the build rendered: the
        // calls stay settled in the ledger, but the response is dropped and the round built again.
        if let Some(build) = current.build.as_ref() {
            let voided = build.voided(build.sources());
            if !voided.is_empty() {
                let key = build.key();
                effects.push(Effect::CancelEngine { run });
                self.rebuild(key, &voided, &pending, effects);
                return Ok(());
            }
        }
        // Only the routes whose rounds the build replays store them; Claude Code keeps its own.
        let Some(connection) = current.route.filter(|route| {
            matches!(
                route.kind,
                kumochi_protocol::message::ConnectionKind::ChatGpt
                    | kumochi_protocol::message::ConnectionKind::AnthropicKey
            )
        }) else {
            return self.fail(run, Failure::Engine, effects);
        };
        let round = current.build.as_ref().map_or(1, |build| build.key().round);
        let calling = current.tools.calling();
        current.finishing = !calling;
        let mut items = std::mem::take(&mut current.tools.items);
        items.sort_by_key(|(index, _)| *index);
        let mut key_calls = std::mem::take(&mut current.tools.key_calls);
        key_calls.sort_by_key(|(index, _)| *index);
        current.tools.item_bytes = 0;
        let mut calls: Vec<_> = std::mem::take(&mut current.tools.open)
            .into_iter()
            .filter_map(|open| Some((open.call, open.output?)))
            .collect();
        let pictured_key_model = current.pictured_key_model().cloned();
        if pictured_key_model.is_some() {
            current.tools.key_results.clone_from(&calls);
            for (_, output) in &mut calls {
                *output = ToolOutput::Text(crate::dispatch::PICTURE_RESULT.to_owned());
            }
        }
        current.tools.dispatched = false;
        current.tools.writing = Some((round, !calling));
        effects.push(Effect::Store(StoreOp::RoundItems(RoundItems {
            thread: current.thread,
            run,
            round,
            connection,
            text: current.unstored.take(),
            items: items
                .into_iter()
                .map(|(_, item)| {
                    if screen {
                        crate::drawing::stored_item(item)
                    } else {
                        item
                    }
                })
                .collect(),
            key_calls: key_calls.into_iter().map(|(_, call)| call).collect(),
            pictured_key_model,
            calls,
        })));
        Ok(())
    }

    /// A round is stored. A late answer for a run that ended only lets the settle pass after
    /// every input write its outcome; otherwise the run's next round is built, or the run ends
    /// after its last.
    pub(super) fn round_stored(
        &mut self,
        run: RunId,
        round: u32,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let found = self.find(run)?;
        let Some((_, last)) = found.tools.writing.filter(|(writing, _)| *writing == round) else {
            return Err(Stale);
        };
        found.tools.writing = None;
        if found.ending.is_some() {
            return Ok(());
        }
        if last {
            self.complete(run, effects)?;
        } else {
            self.build(run, round + 1, effects);
        }
        Ok(())
    }
}
