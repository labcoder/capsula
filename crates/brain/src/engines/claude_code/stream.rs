//! Claude Code's `stream-json` stdout, framed into lines under one budget and mapped one line at a
//! time, so the launch can audit `system:init` before it passes the next line on.
//!
//! Text comes only from `stream_event` deltas, which Claude Code sends only with
//! `--include-partial-messages`; without that flag a question maps to an empty answer.
//! `assistant` messages repeat the streamed text and are ignored.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::super::{ClaudeStreamFailure, EngineFailure, Finish, Tokens, TurnEvent, Usage};

/// The longest line taken, line feed excluded.
pub(crate) const MAX_LINE: usize = 1 << 20;
/// Every byte of stdout counts, ignored lines included.
pub(crate) const MAX_STDOUT: u64 = 16 << 20;
/// The native requests one question may make: the launch policy's verified `--max-turns 8`.
pub(crate) const MAX_NATIVE_TURNS: u8 = 8;

/// Splits stdout into lines, charging every byte to [`MAX_STDOUT`] before it's framed.
#[derive(Default)]
pub(crate) struct LineFramer {
    buf: Vec<u8>,
    start: usize,
    charged: u64,
}

impl fmt::Debug for LineFramer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineFramer")
            .field("unread", &(self.buf.len() - self.start))
            .field("charged", &self.charged)
            .finish_non_exhaustive()
    }
}

impl LineFramer {
    /// Takes the next bytes read from stdout.
    ///
    /// # Errors
    /// [`ClaudeStreamFailure::Budget`] once stdout passes [`MAX_STDOUT`].
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), ClaudeStreamFailure> {
        self.charged += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.charged > MAX_STDOUT {
            return Err(ClaudeStreamFailure::Budget);
        }
        self.buf.drain(..self.start);
        self.start = 0;
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// The next complete line, without its line feed or a carriage return before it.
    ///
    /// # Errors
    /// [`ClaudeStreamFailure::LineTooLong`] for a line over [`MAX_LINE`], complete or not.
    pub(crate) fn next_line(&mut self) -> Result<Option<&[u8]>, ClaudeStreamFailure> {
        let rest = &self.buf[self.start..];
        let Some(end) = rest.iter().position(|&byte| byte == b'\n') else {
            if rest.len() > MAX_LINE {
                return Err(ClaudeStreamFailure::LineTooLong);
            }
            return Ok(None);
        };
        if end > MAX_LINE {
            return Err(ClaudeStreamFailure::LineTooLong);
        }
        let line = &self.buf[self.start..self.start + end];
        self.start += end + 1;
        Ok(Some(line.strip_suffix(b"\r").unwrap_or(line)))
    }

    /// Whether a partial line is left: at EOF, the stream was cut.
    pub(crate) fn has_partial(&self) -> bool {
        self.start < self.buf.len()
    }
}

/// What one line meant. Its `Debug` is structural, as each payload's is.
#[derive(Debug, PartialEq)]
pub(crate) enum Mapped {
    /// Part of the answer.
    Event(TurnEvent),
    /// The question ended: send `Usage` (when the result carried it), then `Finished(finish)`.
    End {
        /// The question's tokens.
        usage: Option<Usage>,
        /// How it ended.
        finish: Finish,
    },
    /// An answer to one of the brain's control requests, for the audit before input.
    Control(ControlResponse),
    /// The facts Claude Code reports once the first user message arrives, for the audit after it.
    Init(InitFacts),
    /// Nothing to act on.
    Ignored,
}

/// A control response: which request it answers and its body, which the audit reads.
#[derive(PartialEq, Deserialize)]
pub(crate) struct ControlResponse {
    /// `success` or `error`.
    pub subtype: String,
    /// The brain's request id.
    pub request_id: String,
    /// The body, absent on an error.
    #[serde(default)]
    pub response: ControlBody,
}

/// A control response's body. `account` is read straight into [`Account`] as the line is parsed,
/// so its email and organization are never held; every other field is kept as sent.
#[derive(Default, PartialEq)]
pub(crate) struct ControlBody {
    /// `initialize`'s `account`; `None` when the body has none.
    pub account: Option<Account>,
    /// Every other field, as an object.
    pub rest: Value,
}

/// `initialize`'s `account`, by the fields the audit judges; the rest are skipped unread.
#[derive(Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Account {
    #[serde(default)]
    pub api_provider: Field,
    #[serde(default)]
    pub token_source: Field,
    #[serde(default)]
    pub api_key_source: Field,
    #[serde(default)]
    pub subscription_type: Field,
}

/// One `account` field as sent, so the audit can tell absent from null and a string from another
/// type.
#[derive(Default, PartialEq)]
pub(crate) enum Field {
    #[default]
    Absent,
    Null,
    Text(String),
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Null => Self::Null,
            Value::String(text) => Self::Text(text),
            _ => Self::Other,
        })
    }
}

impl<'de> Deserialize<'de> for ControlBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(BodyVisitor)
    }
}

/// An [`Account`] that must be a JSON object: serde's derive alone also takes an array.
struct AccountObject(Account);

impl<'de> Deserialize<'de> for AccountObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(AccountVisitor).map(Self)
    }
}

struct AccountVisitor;

impl<'de> Visitor<'de> for AccountVisitor {
    type Value = Account;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an account object")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Account, A::Error> {
        Account::deserialize(de::value::MapAccessDeserializer::new(map))
    }
}

struct BodyVisitor;

impl<'de> Visitor<'de> for BodyVisitor {
    type Value = ControlBody;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a control response body")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ControlBody, A::Error> {
        let mut account = None;
        let mut rest = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "account" {
                if account.is_some() {
                    return Err(de::Error::duplicate_field("account"));
                }
                account = Some(map.next_value::<AccountObject>()?.0);
            } else {
                rest.insert(key, map.next_value()?);
            }
        }
        Ok(ControlBody {
            account,
            rest: Value::Object(rest),
        })
    }
}

/// `system:init`'s audited fields; nothing here is judged until the audit.
#[derive(PartialEq, Deserialize)]
pub(crate) struct InitFacts {
    /// Claude Code's version.
    pub claude_code_version: String,
    /// Every tool the model may call.
    pub tools: Vec<String>,
    /// MCP servers and their status.
    pub mcp_servers: Vec<McpServer>,
    /// Plugins by name and path.
    pub plugins: Vec<Plugin>,
    /// Skills.
    pub skills: Vec<String>,
    /// Slash commands.
    pub slash_commands: Vec<String>,
    /// Where the credential came from.
    #[serde(rename = "apiKeySource")]
    pub api_key_source: String,
    /// Agents.
    pub agents: Vec<String>,
    /// Whether Claude Code reports its analytics as off; shown, never a reason to refuse the route.
    /// Missing or not a boolean reads as `None`.
    #[serde(default, deserialize_with = "bool_or_none")]
    pub analytics_disabled: Option<bool>,
}

fn bool_or_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    Ok(Value::deserialize(deserializer)?.as_bool())
}

/// One MCP server in `system:init`.
#[derive(PartialEq, Deserialize)]
pub(crate) struct McpServer {
    /// Its name.
    pub name: String,
    /// `connected`, `failed` and so on.
    pub status: String,
}

/// One plugin in `system:init`.
#[derive(PartialEq, Deserialize)]
pub(crate) struct Plugin {
    /// Its name.
    pub name: String,
    /// `builtin` for the ones Claude Code ships.
    pub path: String,
}

/// A JSON value's kind and size, never its content.
struct Shape<'a>(&'a Value);

impl fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Null => f.write_str("null"),
            Value::Bool(_) => f.write_str("bool"),
            Value::Number(_) => f.write_str("number"),
            Value::String(text) => write!(f, "string(len={})", text.len()),
            Value::Array(items) => write!(f, "array(len={})", items.len()),
            Value::Object(fields) => write!(f, "object(len={})", fields.len()),
        }
    }
}

/// A string's length, never its content.
struct Len<'a>(&'a str);

impl fmt::Debug for Len<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "len={}", self.0.len())
    }
}

impl fmt::Debug for ControlResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlResponse")
            .field("subtype", &Len(&self.subtype))
            .field("request_id", &Len(&self.request_id))
            .field("account", &self.response.account)
            .field("response", &Shape(&self.response.rest))
            .finish()
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("api_provider", &self.api_provider)
            .field("token_source", &self.token_source)
            .field("api_key_source", &self.api_key_source)
            .field("subscription_type", &self.subscription_type)
            .finish()
    }
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("absent"),
            Self::Null => f.write_str("null"),
            Self::Text(text) => write!(f, "text(len={})", text.len()),
            Self::Other => f.write_str("other"),
        }
    }
}

impl fmt::Debug for InitFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitFacts")
            .field("claude_code_version", &Len(&self.claude_code_version))
            .field("tools", &self.tools.len())
            .field("mcp_servers", &self.mcp_servers)
            .field("plugins", &self.plugins)
            .field("skills", &self.skills.len())
            .field("slash_commands", &self.slash_commands.len())
            .field("api_key_source", &Len(&self.api_key_source))
            .field("agents", &self.agents.len())
            .field("analytics_disabled", &self.analytics_disabled)
            .finish()
    }
}

impl fmt::Debug for McpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &Len(&self.name))
            .field("status", &Len(&self.status))
            .finish()
    }
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &Len(&self.name))
            .field("path", &Len(&self.path))
            .finish()
    }
}

/// Maps lines to turn events for one question.
#[derive(Debug, Default)]
pub(crate) struct StreamMapper {
    native_turns: u8,
    first: Option<Tokens>,
    ended: bool,
    policy_broken: bool,
    unknown: u32,
}

impl StreamMapper {
    /// Maps one complete line.
    ///
    /// # Errors
    /// [`ClaudeStreamFailure::NotJson`] for a line that isn't JSON, and
    /// [`ClaudeStreamFailure::Mistyped`] for a known type with the wrong shape.
    pub(crate) fn line(&mut self, line: &[u8]) -> Result<Mapped, ClaudeStreamFailure> {
        if self.ended || line.is_empty() {
            return Ok(Mapped::Ignored);
        }
        // Checked whole, since serde skips the strings of fields it ignores without checking them.
        let line = std::str::from_utf8(line).map_err(|_| ClaudeStreamFailure::NotJson)?;
        let head: Head<'_> = parse(line)?;
        Ok(match head.kind {
            "stream_event" => self.stream_event(parse::<Wrapped>(line)?.event),
            "result" => self.result(parse(line)?),
            "system" if head.subtype == Some("init") => Mapped::Init(parse(line)?),
            "control_response" => Mapped::Control(parse::<ControlLine>(line)?.response),
            "system" | "assistant" | "user" => Mapped::Ignored,
            _ => {
                self.unknown = self.unknown.saturating_add(1);
                Mapped::Ignored
            }
        })
    }

    /// At EOF: `Finished(Truncated)` unless a result already ended the question.
    pub(crate) fn finish(&mut self) -> Option<TurnEvent> {
        (!std::mem::replace(&mut self.ended, true))
            .then_some(TurnEvent::Finished(Finish::Truncated))
    }

    /// Whether a 9th native request appeared, which means the version's turn cap didn't hold.
    pub(crate) const fn policy_broken(&self) -> bool {
        self.policy_broken
    }

    /// Lines of a type this mapper doesn't know.
    pub(crate) const fn unknown(&self) -> u32 {
        self.unknown
    }

    fn stream_event(&mut self, event: StreamEvent) -> Mapped {
        match event {
            StreamEvent::MessageStart { message } => {
                self.native_turns += 1;
                if self.native_turns > MAX_NATIVE_TURNS {
                    self.policy_broken = true;
                    self.ended = true;
                    return Mapped::End {
                        usage: None,
                        finish: Finish::Failed(EngineFailure::TurnCap),
                    };
                }
                if self.native_turns == 1 {
                    self.first = Some(message.usage.tokens());
                }
                Mapped::Ignored
            }
            StreamEvent::MessageDelta { usage } => {
                if let (1, Some(first), Some(usage)) = (self.native_turns, &mut self.first, usage) {
                    first.output = usage.output_tokens;
                }
                Mapped::Ignored
            }
            StreamEvent::ContentBlockDelta {
                delta: Delta::Text { text },
            } => Mapped::Event(TurnEvent::TextDelta(text)),
            StreamEvent::ContentBlockDelta {
                delta: Delta::Other,
            }
            | StreamEvent::Other => Mapped::Ignored,
        }
    }

    fn result(&mut self, result: ResultLine) -> Mapped {
        self.ended = true;
        let finish = match result.subtype.as_str() {
            // The cap working, not broken: the policy stays as it is.
            "error_max_turns" => Finish::Failed(EngineFailure::TurnCap),
            "success" if !result.is_error => Finish::Completed,
            _ => Finish::Failed(EngineFailure::Provider),
        };
        Mapped::End {
            usage: result.usage.map(|whole| Usage {
                whole: whole.tokens(),
                first_request: self.first,
            }),
            finish,
        }
    }
}

fn parse<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, ClaudeStreamFailure> {
    serde_json::from_str(line).map_err(|error| match error.classify() {
        serde_json::error::Category::Data => ClaudeStreamFailure::Mistyped,
        _ => ClaudeStreamFailure::NotJson,
    })
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    subtype: Option<&'a str>,
}

#[derive(Deserialize)]
struct Wrapped {
    event: StreamEvent,
}

#[derive(Deserialize)]
struct ControlLine {
    response: ControlResponse,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    MessageStart {
        message: StartMessage,
    },
    MessageDelta {
        usage: Option<DeltaUsage>,
    },
    ContentBlockDelta {
        delta: Delta,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StartMessage {
    usage: WireTokens,
}

#[derive(Deserialize)]
struct DeltaUsage {
    output_tokens: u32,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ResultLine {
    subtype: String,
    #[serde(default)]
    is_error: bool,
    usage: Option<WireTokens>,
}

#[derive(Deserialize)]
struct WireTokens {
    #[serde(default, rename = "input_tokens")]
    input: u32,
    #[serde(default, rename = "output_tokens")]
    output: u32,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: u32,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_write: u32,
}

impl WireTokens {
    const fn tokens(&self) -> Tokens {
        Tokens {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        }
    }
}

#[cfg(test)]
mod tests;
