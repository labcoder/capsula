//! The Messages body for a built request: the store's input as user and assistant content
//! blocks behind one cached system block (docs/development/anthropic-messages.md, "The request
//! body").

use std::collections::BTreeSet;

use kumochi_protocol::message::ConnectionKind;
use serde_json::{Map, Value, json};

use crate::connections::model::serialized_len;
use crate::context::{BOUND, InputItem, ModelRequest, Role};
use crate::engines::responses::effort;
use crate::engines::{CallId, Opaque};

/// Every request's output cap, including thinking; the transport's envelope requires it.
const MAX_TOKENS: u64 = 2048;
/// What a developer line starts with, since Messages has no developer role. A label for the
/// model, never an authority: a user who types it is still the user.
const NOTE: &str = "Kumochi note:\n";
/// The longest tool name or call id this route sends, as `CallId` bounds ids.
const NAME_BYTES: usize = 64;

/// Why no body was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BodyError {
    /// The request names no model, because no catalog had been read.
    #[error("the request names no model")]
    NoModel,
    /// The request's model isn't on the Anthropic key.
    #[error("the request's model is on another connection")]
    Route,
    /// The system block would be empty, and Messages refuses an empty text block.
    #[error("the request has no instructions")]
    NoInstructions,
    /// A stored call's arguments aren't one JSON object, which a `tool_use` input must be.
    #[error("a stored call's arguments aren't a JSON object")]
    Arguments,
    /// A call id or tool name isn't 1 to 64 ASCII letters, digits, `_` or `-`.
    #[error("a call id or tool name Messages refuses")]
    Name,
    /// A stored native item isn't a text or `tool_use` block of the fields this route writes.
    #[error("a stored native item isn't a text or tool_use block")]
    Native,
    /// A call isn't answered by exactly one result first in the next user message, or a result
    /// answers no such call.
    #[error("a call and its result aren't paired")]
    Pairing,
    /// An image reached a route that doesn't carry them yet.
    #[error("this route can't carry images")]
    Image,
    /// The serialised body is over the request's bound.
    #[error("the request body is over its size bound")]
    TooLarge,
}

/// The Messages body for `request`: the instructions as the one system block, which ends
/// the cached prefix after the tools; then the input, neighbouring blocks of one role merged into
/// one message, as Messages wants roles to alternate.
pub(crate) fn body(request: &ModelRequest) -> Result<Value, BodyError> {
    let choice = request.choice.as_ref().ok_or(BodyError::NoModel)?;
    if choice.connection != ConnectionKind::AnthropicKey {
        return Err(BodyError::Route);
    }
    if request.instructions.is_empty() {
        return Err(BodyError::NoInstructions);
    }
    let mut messages: Vec<(Role, Vec<Value>)> = Vec::new();
    for item in &request.input {
        let (role, block) = match item {
            InputItem::Message { text, .. } if text.is_empty() => continue,
            InputItem::Message { role, text } => match role {
                Role::User => (Role::User, text_block(text)),
                Role::Developer => (Role::User, text_block(&format!("{NOTE}{text}"))),
                Role::Assistant => (Role::Assistant, text_block(text)),
            },
            InputItem::FunctionCall {
                call,
                name,
                arguments,
            } => {
                let input = serde_json::from_str::<Value>(arguments)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or(BodyError::Arguments)?;
                (Role::Assistant, tool_use(id(call)?, named(name)?, &input))
            }
            InputItem::FunctionOutput { call, output } => {
                let block =
                    json!({ "type": "tool_result", "tool_use_id": id(call)?, "content": output });
                (Role::User, block)
            }
            // Only this connection's own items reach here; the build filters the rest.
            InputItem::Native(item) => (Role::Assistant, native(item)?),
            InputItem::Image(_) => return Err(BodyError::Image),
        };
        match messages.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => messages.push((role, vec![block])),
        }
    }
    paired(&messages)?;
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| {
            let role = if role == Role::Assistant {
                "assistant"
            } else {
                "user"
            };
            json!({ "role": role, "content": content })
        })
        .collect();
    let mut body = json!({
        "model": choice.model.as_str(),
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [{
            "type": "text",
            "text": request.instructions,
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": messages,
        "output_config": { "effort": effort(choice.effort) },
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
    }
    if serialized_len(&body) > BOUND {
        return Err(BodyError::TooLarge);
    }
    Ok(body)
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_use(id: &str, name: &str, input: &Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// A stored native item rebuilt from the only fields this route writes: a text block with its
/// text, or a `tool_use` block with its id, name and object input. Anything else, a thinking,
/// image or document block among them, builds nothing.
fn native(item: &Opaque) -> Result<Value, BodyError> {
    let block: Map<String, Value> =
        serde_json::from_slice(item.bytes()).map_err(|_| BodyError::Native)?;
    let field = |key: &str| block.get(key).ok_or(BodyError::Native);
    match (field("type")?.as_str(), block.len()) {
        (Some("text"), 2) => {
            let text = field("text")?.as_str().filter(|text| !text.is_empty());
            Ok(text_block(text.ok_or(BodyError::Native)?))
        }
        (Some("tool_use"), 4) => {
            let id = field("id")?.as_str().ok_or(BodyError::Native)?;
            let name = field("name")?.as_str().ok_or(BodyError::Native)?;
            let input = field("input")?;
            if !input.is_object() {
                return Err(BodyError::Native);
            }
            Ok(tool_use(named(id)?, named(name)?, input))
        }
        _ => Err(BodyError::Native),
    }
}

/// Each assistant message's calls are answered exactly once, by the results that open the next
/// message, which must be the user's; no id is called twice and no result stands elsewhere.
fn paired(messages: &[(Role, Vec<Value>)]) -> Result<(), BodyError> {
    let mut called = BTreeSet::new();
    let mut open: Vec<&str> = Vec::new();
    for (_, blocks) in messages {
        let results = blocks
            .iter()
            .take_while(|block| block["type"] == "tool_result")
            .count();
        let mut answered: Vec<&str> = blocks[..results]
            .iter()
            .filter_map(|block| block["tool_use_id"].as_str())
            .collect();
        answered.sort_unstable();
        open.sort_unstable();
        if answered != open {
            return Err(BodyError::Pairing);
        }
        open.clear();
        for block in &blocks[results..] {
            match block["type"].as_str() {
                Some("tool_result") => return Err(BodyError::Pairing),
                Some("tool_use") => {
                    let id = block["id"].as_str().ok_or(BodyError::Pairing)?;
                    if !called.insert(id) {
                        return Err(BodyError::Pairing);
                    }
                    open.push(id);
                }
                _ => {}
            }
        }
    }
    if open.is_empty() {
        Ok(())
    } else {
        Err(BodyError::Pairing)
    }
}

fn id(call: &CallId) -> Result<&str, BodyError> {
    named(call.as_str())
}

/// `name`, if Messages takes it as a tool name or a `tool_use` id.
fn named(name: &str) -> Result<&str, BodyError> {
    let valid = !name.is_empty()
        && name.len() <= NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    valid.then_some(name).ok_or(BodyError::Name)
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;
