//! The Messages body for a built request: the store's input as user and assistant content
//! blocks behind one cached system block (docs/development/anthropic-messages.md, "The request
//! body").

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use kumochi_protocol::message::{ConnectionKind, ModelSlug};
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use super::{Mapper, Reply};
use crate::connections::anthropic::messages::MessagesBody;
use crate::connections::anthropic::request::{literal, members};
use crate::connections::anthropic::sse_map::Capture;
use crate::context::{BOUND, ConnectionTag, InputItem, ModelRequest, Role};
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
    /// The conversation is empty or ends with the assistant, which would ask for a prefill.
    #[error("the conversation doesn't end with the user")]
    Ending,
    /// The transport's envelope check refused the body.
    #[error("the transport's envelope refuses the body")]
    Envelope,
    /// The reply answered another request, or the next one would go on another sign-in or
    /// model, so its signed thinking can't go back.
    #[error("the reply answered another conversation")]
    Prefix,
}

/// The exact request a run last sent on this route, its messages as the JSON sent. A tool round
/// only appends to it, since a signed thinking block is bound to everything before it; it lives
/// in the run's memory only.
pub(crate) struct Conversation {
    head: Map<String, Value>,
    messages: Vec<String>,
    tag: ConnectionTag,
    prefix: Prefix,
}

impl Conversation {
    /// The body's JSON: the head's fields, then the messages as they were written.
    fn wire(&self) -> Result<Vec<u8>, BodyError> {
        let mut bytes = serde_json::to_vec(&self.head).map_err(|_| BodyError::Envelope)?;
        bytes.pop();
        bytes.extend_from_slice(b",\"messages\":[");
        bytes.extend_from_slice(self.messages.join(",").as_bytes());
        bytes.extend_from_slice(b"]}");
        Ok(bytes)
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("messages", &self.messages.len())
            .field("tag", &self.tag)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// One built request, unique in the process: a rebuild of the same input is another one. It
/// means built, nothing more; whether the run may still send is its owner's to say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Prefix(u64);

static PREFIXES: AtomicU64 = AtomicU64::new(0);

impl Mapper {
    /// A mapper for the stream answering `conversation`'s last request, made before the stream's
    /// first callback: only its successful completion yields a reply that continues it.
    pub(crate) fn answering(conversation: &Conversation) -> Self {
        Self {
            answering: Some(Prefix(conversation.prefix.0)),
            ..Self::default()
        }
    }

    /// What this mapper's stream keeps beside its parsed events: block starts only when bound.
    pub(crate) fn capture(&self) -> Capture {
        if self.answering.is_some() {
            Capture::BlockStarts
        } else {
            Capture::Nothing
        }
    }
}

/// The Messages body for `request` on the sign-in `tag`, and the conversation a tool round
/// continues. Stored items never carry thinking, so neither does this body.
pub(crate) fn body(
    request: &ModelRequest,
    tag: ConnectionTag,
) -> Result<(MessagesBody, Conversation), BodyError> {
    let (Value::Object(mut head), messages) = json(request)? else {
        return Err(BodyError::Envelope);
    };
    head.remove("messages");
    sealed(head, messages, tag)
}

/// The next request of a tool loop: `conversation` unchanged, then `reply` as the assistant's
/// message, exactly as its mapper kept it, then `results`, one for each of its calls. Only the
/// reply to this very request, sent on the same sign-in and model, continues it.
pub(crate) fn next(
    conversation: Conversation,
    tag: ConnectionTag,
    model: &ModelSlug,
    reply: Reply,
    results: &[(CallId, String)],
) -> Result<(MessagesBody, Conversation), BodyError> {
    let Conversation {
        head,
        mut messages,
        tag: built,
        prefix,
    } = conversation;
    let Reply {
        prefix: answered,
        blocks,
    } = reply;
    if answered != prefix || tag != built || head.get("model") != Some(&Value::from(model.as_str()))
    {
        return Err(BodyError::Prefix);
    }
    let mut called = false;
    for block in &blocks {
        let block = literal(block.as_bytes()).map_err(|_| BodyError::Native)?;
        if block["type"] == "tool_use" {
            named(block["id"].as_str().ok_or(BodyError::Name)?)?;
            named(block["name"].as_str().ok_or(BodyError::Name)?)?;
            called = true;
        }
    }
    if !called {
        return Err(BodyError::Pairing);
    }
    let assistant = format!(r#"{{"role":"assistant","content":[{}]}}"#, blocks.join(","));
    messages.push(assistant);
    let results = results
        .iter()
        .map(|(call, output)| Ok(tool_result(id(call)?, output)))
        .collect::<Result<Vec<_>, BodyError>>()?;
    messages.push(written(&json!({ "role": "user", "content": results }))?);
    sealed(head, messages, tag)
}

fn written(value: &Value) -> Result<String, BodyError> {
    serde_json::to_string(value).map_err(|_| BodyError::Envelope)
}

/// Checks a whole body and hands its exact bytes to the transport's envelope.
fn sealed(
    head: Map<String, Value>,
    messages: Vec<String>,
    tag: ConnectionTag,
) -> Result<(MessagesBody, Conversation), BodyError> {
    // Read back only to check pairs and roles; what's sent is `messages` as written.
    let read = messages
        .iter()
        .map(|message| literal(message.as_bytes()).map_err(|_| BodyError::Envelope))
        .collect::<Result<Vec<Value>, _>>()?;
    paired(&read)?;
    if read.last().is_none_or(|last| last["role"] != "user") {
        return Err(BodyError::Ending);
    }
    let conversation = Conversation {
        head,
        messages,
        tag,
        prefix: Prefix(PREFIXES.fetch_add(1, Ordering::Relaxed)),
    };
    let bytes = conversation.wire()?;
    if bytes.len() > BOUND {
        return Err(BodyError::TooLarge);
    }
    let sent = MessagesBody::from_json(Zeroizing::new(bytes)).map_err(|_| BodyError::Envelope)?;
    Ok((sent, conversation))
}

/// The request's JSON, its messages written beside it: the instructions as the one system block,
/// which ends the cached prefix after the tools; then the input, neighbouring blocks of one role
/// merged into one message, as Messages wants roles to alternate.
fn json(request: &ModelRequest) -> Result<(Value, Vec<String>), BodyError> {
    let choice = request.choice.as_ref().ok_or(BodyError::NoModel)?;
    if choice.connection != ConnectionKind::AnthropicKey {
        return Err(BodyError::Route);
    }
    if request.instructions.is_empty() {
        return Err(BodyError::NoInstructions);
    }
    let mut messages: Vec<(Role, Vec<String>)> = Vec::new();
    for item in &request.input {
        let (role, block) = match item {
            InputItem::Message { text, .. } if text.is_empty() => continue,
            InputItem::Message { role, text } => match role {
                Role::User => (Role::User, written(&text_block(text))?),
                Role::Developer => (Role::User, written(&text_block(&format!("{NOTE}{text}")))?),
                Role::Assistant => (Role::Assistant, written(&text_block(text))?),
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
                let block = tool_use(id(call)?, named(name)?, &input);
                (Role::Assistant, written(&block)?)
            }
            InputItem::FunctionOutput { call, output } => {
                (Role::User, written(&tool_result(id(call)?, output))?)
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
    let messages = messages
        .into_iter()
        .map(|(role, content)| {
            let role = if role == Role::Assistant {
                "assistant"
            } else {
                "user"
            };
            format!(r#"{{"role":"{role}","content":[{}]}}"#, content.join(","))
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
        "messages": [],
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
    Ok((body, messages))
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_use(id: &str, name: &str, input: &Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

fn tool_result(id: &str, output: &str) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": output })
}

/// A stored native item as it goes back: a `tool_use` block exactly as [`ToolUse`] keeps it, or
/// a text block with its text. Anything else, a thinking, image or document block among them,
/// builds nothing: stored items never carry thinking.
fn native(item: &Opaque) -> Result<String, BodyError> {
    let block = literal(item.bytes()).map_err(|_| BodyError::Native)?;
    let field = |name: &str| block.get(name).and_then(Value::as_str);
    match (
        field("type"),
        block.as_object().map(Map::len),
        field("text"),
    ) {
        (Some("text"), Some(2), Some(text)) if !text.is_empty() => written(&text_block(text)),
        (Some("tool_use"), Some(4), _) if block["input"].is_object() => {
            named(field("id").ok_or(BodyError::Native)?)?;
            named(field("name").ok_or(BodyError::Native)?)?;
            let call = ToolUse::stored(item.bytes()).ok_or(BodyError::Native)?;
            Ok(call.json())
        }
        _ => Err(BodyError::Native),
    }
}

/// A call of the key's reply as its rounds store and replay it: the four fields of a `tool_use`
/// block, its input the JSON text exactly as the reader read it. Only a call this encoder sends
/// back can be made, so a stored one always encodes again.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolUse {
    id: String,
    name: String,
    input: String,
}

impl fmt::Debug for ToolUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolUse")
            .field("input_bytes", &self.input.len())
            .finish_non_exhaustive()
    }
}

impl ToolUse {
    /// The call in a reply's `tool_use` block: its four fields and nothing else the block holds.
    pub(crate) fn projected(block: &str) -> Option<Self> {
        Self::read(&fields(block)?)
    }

    /// A stored call, only as [`ToolUse::json`] writes one: those four members and no other.
    pub(crate) fn stored(item: &[u8]) -> Option<Self> {
        let fields = fields(std::str::from_utf8(item).ok()?)?;
        if fields.len() != 4 {
            return None;
        }
        Self::read(&fields)
    }

    fn read(fields: &[(String, &str)]) -> Option<Self> {
        let field = |name: &str| {
            fields
                .iter()
                .find(|(named, _)| named == name)
                .map(|(_, value)| *value)
        };
        let text = |name| serde_json::from_str::<String>(field(name)?).ok();
        let input = field("input")?;
        members(input)?;
        let (id, name) = (text("id")?, text("name")?);
        (text("type")? == "tool_use" && named(&id).is_ok() && named(&name).is_ok()).then(|| Self {
            id,
            name,
            input: input.to_owned(),
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The block as stored and sent: an id and a name need no escaping, and the input goes as read.
    pub(crate) fn json(&self) -> String {
        format!(
            r#"{{"type":"tool_use","id":"{}","name":"{}","input":{}}}"#,
            self.id, self.name, self.input
        )
    }
}

/// `text`'s members, each value as written, when it is one object [`literal`] reads.
fn fields(text: &str) -> Option<Vec<(String, &str)>> {
    literal(text.as_bytes()).ok()?;
    members(text)
}

/// Each assistant message's calls are answered exactly once, by the results that open the next
/// message, which must be the user's; no id is called twice and no result stands elsewhere.
fn paired(messages: &[Value]) -> Result<(), BodyError> {
    let mut called = BTreeSet::new();
    let mut open: Vec<&str> = Vec::new();
    for message in messages {
        let blocks = message["content"].as_array().ok_or(BodyError::Pairing)?;
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
