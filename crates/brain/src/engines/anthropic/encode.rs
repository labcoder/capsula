//! The Messages body for a built request: the store's input as user and assistant content
//! blocks behind one cached system block (docs/development/anthropic-messages.md, "The request
//! body").

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kumochi_protocol::message::{ConnectionKind, ModelSlug};
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use super::{Mapper, Reply};
use crate::connections::anthropic::messages::MessagesBody;
use crate::connections::anthropic::request::{literal, members, object_within_depth};
use crate::connections::anthropic::sse_map::Capture;
use crate::connections::model::MAX_REQUEST_BYTES;
use crate::context::{BOUND, ConnectionTag, InputItem, Jpeg, ModelRequest, Role};
use crate::coordinator::PictureRoutes;
use crate::engines::responses::effort;
use crate::engines::{CallId, Opaque};
use crate::screen::JPEG_CAP;

/// Every request's output cap, including thinking; the transport's envelope requires it.
const MAX_TOKENS: u64 = 2048;
/// What a developer line starts with, since Messages has no developer role. A label for the
/// model, never an authority: a user who types it is still the user.
const NOTE: &str = "Kumochi note:\n";
/// The longest tool name or call id this route sends, as `CallId` bounds ids.
const NAME_BYTES: usize = 64;
/// Tool-input containers, including the root object. Leaves headroom for the five enclosing
/// request containers under `serde_json`'s recursion limit.
const MAX_TOOL_INPUT_DEPTH: usize = 96;
const IMAGE_START: &[u8] =
    br#"{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":""#;
const IMAGE_END: &[u8] = br#""}}"#;
// A body at its bound with the largest picture still fits the transport's request cap.
const _: () = assert!(
    BOUND + 1 + IMAGE_START.len() + JPEG_CAP.div_ceil(3) * 4 + IMAGE_END.len() <= MAX_REQUEST_BYTES,
    "a bound body with the largest picture is over the request cap"
);

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
    /// An image this body can't carry: pictures are off for this route, the image isn't one
    /// sealed capture within its cap, it's a second one, or a result would follow it in its
    /// message.
    #[error("this body can't carry the image")]
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
    picture: Option<Picture>,
}

/// The one picture a body carries, kept out of `messages` so no plain string ever holds it: the
/// sealed JPEG, and where in its message, and between which commas, its block goes.
struct Picture {
    jpeg: Jpeg,
    message: usize,
    at: usize,
    before: &'static [u8],
    after: &'static [u8],
}

impl Picture {
    fn len(&self, jpeg: &[u8]) -> usize {
        self.before.len()
            + IMAGE_START.len()
            + jpeg.len().div_ceil(3) * 4
            + IMAGE_END.len()
            + self.after.len()
    }

    /// Writes the block into `out`, exactly [`Self::len`] bytes.
    fn write(&self, jpeg: &[u8], out: &mut [u8]) -> Result<(), BodyError> {
        let (head, rest) = out.split_at_mut(self.before.len() + IMAGE_START.len());
        let (data, end) = rest.split_at_mut(rest.len() - IMAGE_END.len() - self.after.len());
        head[..self.before.len()].copy_from_slice(self.before);
        head[self.before.len()..].copy_from_slice(IMAGE_START);
        STANDARD
            .encode_slice(jpeg, data)
            .map_err(|_| BodyError::Image)?;
        end[..IMAGE_END.len()].copy_from_slice(IMAGE_END);
        end[IMAGE_END.len()..].copy_from_slice(self.after);
        Ok(())
    }
}

impl Conversation {
    /// The body's JSON: the head's fields, then the messages as they were written, and where in
    /// it the picture goes.
    fn wire(&self) -> Result<(Vec<u8>, Option<usize>), BodyError> {
        let mut bytes = serde_json::to_vec(&self.head).map_err(|_| BodyError::Envelope)?;
        bytes.pop();
        bytes.extend_from_slice(b",\"messages\":[");
        let mut at = None;
        for (index, message) in self.messages.iter().enumerate() {
            if index > 0 {
                bytes.push(b',');
            }
            if let Some(picture) = self.picture.as_ref().filter(|p| p.message == index) {
                at = Some(bytes.len() + picture.at);
            }
            bytes.extend_from_slice(message.as_bytes());
        }
        bytes.extend_from_slice(b"]}");
        Ok((bytes, at))
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("messages", &self.messages.len())
            .field("tag", &self.tag)
            .field("prefix", &self.prefix)
            .field("pictured", &self.picture.is_some())
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
    body_under(request, tag, PictureRoutes::SHIPPED)
}

/// [`body`], its one image carried only while `routes` lets this route carry pictures. The
/// lease's picture grant still decides whether a byte of it is written.
pub(crate) fn body_under(
    request: &ModelRequest,
    tag: ConnectionTag,
    routes: PictureRoutes,
) -> Result<(MessagesBody, Conversation), BodyError> {
    let (Value::Object(mut head), messages, picture) = json(request, routes)? else {
        return Err(BodyError::Envelope);
    };
    head.remove("messages");
    sealed(head, messages, tag, picture)
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
    next_under(
        conversation,
        tag,
        model,
        reply,
        results,
        PictureRoutes::SHIPPED,
    )
}

/// [`next`], a retained picture going again only while `routes` lets this route carry it.
fn next_under(
    conversation: Conversation,
    tag: ConnectionTag,
    model: &ModelSlug,
    reply: Reply,
    results: &[(CallId, String)],
    routes: PictureRoutes,
) -> Result<(MessagesBody, Conversation), BodyError> {
    let Conversation {
        head,
        mut messages,
        tag: built,
        prefix,
        picture,
    } = conversation;
    if picture.is_some() && !routes.allows(ConnectionKind::AnthropicKey) {
        return Err(BodyError::Image);
    }
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
    sealed(head, messages, tag, picture)
}

fn written(value: &Value) -> Result<String, BodyError> {
    serde_json::to_string(value).map_err(|_| BodyError::Envelope)
}

/// Checks a whole body and hands its exact bytes to the transport's envelope. The envelope and
/// the bound see the body without its picture, whose block is then spliced in: fixed JSON and
/// base64, so it needs no escaping and no reparse.
fn sealed(
    head: Map<String, Value>,
    messages: Vec<String>,
    tag: ConnectionTag,
    picture: Option<Picture>,
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
        picture,
    };
    let (bytes, at) = conversation.wire()?;
    if bytes.len() > BOUND {
        return Err(BodyError::TooLarge);
    }
    let mut sent =
        MessagesBody::from_json(Zeroizing::new(bytes)).map_err(|_| BodyError::Envelope)?;
    if let (Some(picture), Some(at)) = (&conversation.picture, at) {
        let (jpeg, _) = picture.jpeg.capture().ok_or(BodyError::Image)?;
        sent = sent
            .spliced(at, picture.len(jpeg), |out| picture.write(jpeg, out))
            .map_err(|_| BodyError::Image)?;
    }
    Ok((sent, conversation))
}

/// The request's JSON, its messages written beside it: the instructions as the one system block,
/// which ends the cached prefix after the tools; then the input, neighbouring blocks of one role
/// merged into one message, as Messages wants roles to alternate.
fn json(
    request: &ModelRequest,
    routes: PictureRoutes,
) -> Result<(Value, Vec<String>, Option<Picture>), BodyError> {
    let choice = request.choice.as_ref().ok_or(BodyError::NoModel)?;
    if choice.connection != ConnectionKind::AnthropicKey {
        return Err(BodyError::Route);
    }
    if request.instructions.is_empty() {
        return Err(BodyError::NoInstructions);
    }
    let mut messages: Vec<(Role, Vec<String>)> = Vec::new();
    let mut placed: Option<(&Jpeg, usize, usize)> = None;
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
                // A result joining the picture's message would follow it, and results lead.
                if placed.is_some_and(|(_, message, _)| message + 1 == messages.len()) {
                    return Err(BodyError::Image);
                }
                (Role::User, written(&tool_result(id(call)?, output))?)
            }
            // Only this connection's own items reach here; the build filters the rest.
            InputItem::Native(item) => (Role::Assistant, native(item)?),
            InputItem::Image(image) => {
                let sealed = image
                    .capture()
                    .is_some_and(|(jpeg, _)| jpeg.len() <= JPEG_CAP);
                if !routes.allows(ConnectionKind::AnthropicKey) || !sealed || placed.is_some() {
                    return Err(BodyError::Image);
                }
                if messages.last().is_none_or(|(role, _)| *role != Role::User) {
                    messages.push((Role::User, Vec::new()));
                }
                let blocks = messages.last().map_or(0, |(_, blocks)| blocks.len());
                placed = Some((image, messages.len() - 1, blocks));
                continue;
            }
        };
        match messages.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => messages.push((role, vec![block])),
        }
    }
    let (written, picture) = messages_written(messages, placed);
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
    Ok((body, written, picture))
}

/// The messages as written, neighbouring blocks already merged, and where among them the placed
/// picture's block goes: its message, and how many blocks precede it there.
fn messages_written(
    messages: Vec<(Role, Vec<String>)>,
    placed: Option<(&Jpeg, usize, usize)>,
) -> (Vec<String>, Option<Picture>) {
    let mut picture = None;
    let mut written = Vec::with_capacity(messages.len());
    for (index, (role, content)) in messages.into_iter().enumerate() {
        let role = if role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let head = format!(r#"{{"role":"{role}","content":["#);
        if let Some((jpeg, _, block)) = placed.filter(|(_, message, _)| *message == index) {
            picture = Some(Picture {
                jpeg: jpeg.clone(),
                message: index,
                at: head.len() + content[..block].join(",").len(),
                before: if block > 0 { b"," } else { b"" },
                after: if block == 0 && !content.is_empty() {
                    b","
                } else {
                    b""
                },
            });
        }
        written.push(format!("{head}{}]}}", content.join(",")));
    }
    (written, picture)
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
        if !object_within_depth::<MAX_TOOL_INPUT_DEPTH>(input) {
            return None;
        }
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
/// message, which must be the user's. An id is unique in its group; no result stands elsewhere.
fn paired(messages: &[Value]) -> Result<(), BodyError> {
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
        let mut called = BTreeSet::new();
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
