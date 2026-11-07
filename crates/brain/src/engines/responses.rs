//! The Responses engine: the request body, serialised from the context builder's
//! [`ModelRequest`], the mapping from one round's stream events to [`TurnEvent`]s, and the
//! engine that sends one through the transport and feeds the other. See
//! docs/development/engines.md.

mod engine;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod tests;

pub(crate) use engine::{Access, AccessAsk, AccessHandle, Bearer, Responses};

use std::fmt;

use kumochi_protocol::message::Effort;
use serde_json::{Value, json};

use super::{CallId, EngineFailure, Finish, Opaque, ToolName, TurnEvent};
use crate::connections::model::error::Failure;
use crate::connections::model::sse::{Progress, SseEvent};
use crate::context::{BOUND, InputItem, ModelRequest, Role};

/// What one round keeps while it streams: its calls' ids and arguments and the native items it
/// passes on [guess].
const ROUND_BYTES: usize = 1024 * 1024;
/// The calls and native items one round may have [guess].
const ROUND_ITEMS: usize = 64;

/// Why no body was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BodyError {
    /// The request names no model, because no catalog had been read.
    #[error("the request names no model")]
    NoModel,
    /// A stored native item isn't JSON.
    #[error("a stored native item isn't JSON")]
    Native,
    /// The serialised body is over the request's bound.
    #[error("the request body is over its size bound")]
    TooLarge,
}

/// The Responses body for `request`: nothing is stored on the provider's side, the answer
/// streams, and reasoning comes back encrypted so it can be replayed.
pub(crate) fn body(request: &ModelRequest) -> Result<Value, BodyError> {
    let choice = request.choice.as_ref().ok_or(BodyError::NoModel)?;
    let input = request
        .input
        .iter()
        .map(input_item)
        .collect::<Result<Vec<_>, _>>()?;
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            let mut value = json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            });
            if tool.name == "draw" {
                value["strict"] = Value::Bool(false);
            }
            value
        })
        .collect();
    let body = json!({
        "model": choice.model.as_str(),
        "instructions": request.instructions,
        "input": input,
        "tools": tools,
        "reasoning": { "effort": effort(choice.effort) },
        "include": ["reasoning.encrypted_content"],
        "store": false,
        "stream": true,
    });
    if body.to_string().len() > BOUND {
        return Err(BodyError::TooLarge);
    }
    Ok(body)
}

fn input_item(item: &InputItem) -> Result<Value, BodyError> {
    Ok(match item {
        InputItem::Message { role, text } => {
            let role = match role {
                Role::Developer => "developer",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({ "type": "message", "role": role, "content": text })
        }
        InputItem::FunctionCall {
            call,
            name,
            arguments,
        } => json!({
            "type": "function_call",
            "call_id": call.as_str(),
            "name": name,
            "arguments": arguments,
        }),
        InputItem::FunctionOutput { call, output } => json!({
            "type": "function_call_output",
            "call_id": call.as_str(),
            "output": output,
        }),
        // Parsed only to be sent back as the same JSON value; it's never read or logged.
        InputItem::Native(item) => {
            serde_json::from_slice(item.bytes()).map_err(|_| BodyError::Native)?
        }
    })
}

fn effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// One function call the round started.
struct Started {
    /// The provider's item id, which its argument events name.
    item: String,
    call: CallId,
    /// The name it started with, which its done item must repeat.
    name: String,
    args: String,
    ready: bool,
}

/// One round's stream events, mapped in order. A call's name arrives before its first
/// argument delta, each call is ready once, and every completed output item is passed on for
/// replay. Anything else, or a round over its budget, fails the round as `Protocol`.
#[derive(Default)]
pub(crate) struct Round {
    calls: Vec<Started>,
    items: usize,
    bytes: usize,
    events: Vec<TurnEvent>,
    broken: bool,
}

impl fmt::Debug for Round {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Round")
            .field("calls", &self.calls.len())
            .field("items", &self.items)
            .field("bytes", &self.bytes)
            .field("events", &self.events.len())
            .field("broken", &self.broken)
            .finish()
    }
}

/// The event broke the round's rules.
struct Broken;

impl Round {
    /// Maps one event; after the round broke, nothing more.
    pub(crate) fn event(&mut self, event: &SseEvent) {
        if !self.broken && self.map(event).is_err() {
            self.broken = true;
        }
    }

    /// The events mapped since the last call, in order.
    pub(crate) fn take(&mut self) -> Vec<TurnEvent> {
        std::mem::take(&mut self.events)
    }

    /// The round's ending once the reader read its latest bytes, or `None` while it goes on.
    /// A stream whose bytes run out has no ending here: the pump reports it as truncated.
    pub(crate) fn end(&self, read: Result<Progress, Failure>) -> Option<TurnEvent> {
        let finish = match read {
            _ if self.broken => Finish::Failed(EngineFailure::Stream(Failure::Protocol)),
            Ok(Progress::More) => return None,
            Ok(Progress::Completed) if self.calls.iter().any(|call| !call.ready) => {
                Finish::Failed(EngineFailure::Stream(Failure::Protocol))
            }
            Ok(Progress::Completed) => Finish::Completed,
            Err(failure) => Finish::Failed(EngineFailure::Stream(failure)),
        };
        Some(TurnEvent::Finished(finish))
    }

    fn map(&mut self, event: &SseEvent) -> Result<(), Broken> {
        let value = event.value();
        match event.kind() {
            "response.output_text.delta" => {
                let delta = text(&value["delta"])?;
                self.events.push(TurnEvent::TextDelta(delta.to_owned()));
            }
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                self.started(&value["item"])?;
            }
            "response.function_call_arguments.delta" => {
                let delta = text(&value["delta"])?;
                let at = self.open(&value["item_id"])?;
                self.charge(0, delta.len())?;
                let call = &mut self.calls[at];
                call.args.push_str(delta);
                let call = call.call.clone();
                self.events.push(TurnEvent::ToolArgsDelta {
                    call,
                    delta: delta.to_owned(),
                });
            }
            "response.function_call_arguments.done" => {
                let args = text(&value["arguments"])?;
                let at = self.open(&value["item_id"])?;
                if self.calls[at].args != args {
                    return Err(Broken);
                }
            }
            "response.output_item.done" => self.done(value)?,
            _ => {}
        }
        Ok(())
    }

    fn started(&mut self, item: &Value) -> Result<(), Broken> {
        let id = text(&item["id"])?;
        let call = CallId::new(text(&item["call_id"])?).map_err(|_| Broken)?;
        if self
            .calls
            .iter()
            .any(|known| known.item == id || known.call == call)
        {
            return Err(Broken);
        }
        let name = text(&item["name"])?;
        self.charge(1, id.len() + call.as_str().len() + name.len())?;
        self.events.push(TurnEvent::ToolCallStarted {
            call: call.clone(),
            name: ToolName::from_wire(name),
        });
        self.calls.push(Started {
            item: id.to_owned(),
            call,
            name: name.to_owned(),
            args: String::new(),
            ready: false,
        });
        Ok(())
    }

    /// A completed item: checked whole, kept and charged before either of its events is queued,
    /// so a refused item never makes its call ready.
    fn done(&mut self, value: &Value) -> Result<(), Broken> {
        let index = value["output_index"]
            .as_u64()
            .and_then(|index| u32::try_from(index).ok())
            .ok_or(Broken)?;
        let item = &value["item"];
        let kind = item
            .as_object()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .ok_or(Broken)?;
        let ready = if kind == "function_call" {
            let at = self.open(&item["id"])?;
            let call = &self.calls[at];
            if call.call.as_str() != text(&item["call_id"])?
                || call.name != text(&item["name"])?
                || call.args != text(&item["arguments"])?
            {
                return Err(Broken);
            }
            Some(at)
        } else {
            None
        };
        let bytes = serde_json::to_vec(item).map_err(|_| Broken)?;
        let size = bytes.len();
        let item = Opaque::new(bytes).map_err(|_| Broken)?;
        self.charge(1, size)?;
        if let Some(at) = ready {
            let call = &mut self.calls[at];
            call.ready = true;
            let ready = TurnEvent::ToolCallReady {
                call: call.call.clone(),
                args: call.args.clone(),
            };
            self.events.push(ready);
        }
        self.events.push(TurnEvent::OutputItem { index, item });
        Ok(())
    }

    /// Where the started call `item_id` names is, while it isn't ready.
    fn open(&self, item_id: &Value) -> Result<usize, Broken> {
        let id = text(item_id)?;
        self.calls
            .iter()
            .position(|call| call.item == id && !call.ready)
            .ok_or(Broken)
    }

    /// Charges the round's budget before it keeps anything more.
    fn charge(&mut self, items: usize, bytes: usize) -> Result<(), Broken> {
        if self.items + items > ROUND_ITEMS || self.bytes + bytes > ROUND_BYTES {
            return Err(Broken);
        }
        self.items += items;
        self.bytes += bytes;
        Ok(())
    }
}

fn text(value: &Value) -> Result<&str, Broken> {
    value.as_str().ok_or(Broken)
}
