//! Bounded Messages block assembly; transport and dispatch belong to their existing owners.

pub(crate) mod encode;

use std::{fmt, io};

use serde_json::Value;

use super::{CallId, EngineFailure, Finish, Opaque, Tokens, ToolName, TurnEvent, Usage};
use crate::connections::{
    anthropic::error::Failure,
    model::sse::{Progress, SseEvent},
};

const ITEMS: usize = 64;
const BYTES: usize = 1024 * 1024;
const NAME: usize = 64;
const EVENT_BYTES: usize = 64;

enum Block {
    /// A bound mapper keeps the text for the reply; an unbound one keeps none.
    Text(String),
    Tool {
        call: CallId,
        name: String,
        start: String,
        args: Option<String>,
    },
    Thinking {
        text: String,
        signature: String,
    },
    RedactedThinking(String),
    Skipped,
    Closed,
}

impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text(_) => "Text",
            Self::Tool { .. } => "Tool",
            Self::Thinking { .. } => "Thinking",
            Self::RedactedThinking(_) => "RedactedThinking",
            Self::Skipped => "Skipped",
            Self::Closed => "Closed",
        })
    }
}

/// One round's retained blocks and queued events. A mapper made by `Mapper::answering` for a
/// request also keeps the round's whole reply, which only its successful completion yields.
#[derive(Default)]
pub(crate) struct Mapper {
    blocks: Vec<(u32, Block)>,
    ids: Vec<CallId>,
    events: Vec<TurnEvent>,
    bytes: usize,
    usage: Option<Tokens>,
    final_usage: bool,
    broken: bool,
    terminal: bool,
    /// The request this stream answers, set before its first callback and cleared once the reply
    /// can't continue it.
    answering: Option<encode::Prefix>,
    /// Each stopped block of the reply, as its JSON, by output index.
    kept: Vec<(u32, String)>,
}

/// A completed round's whole reply, its blocks in output order exactly as they came (tool input as
/// its streamed text). Only the mapper bound to a request makes one, for that request alone; it
/// lives in the run's memory and is never stored.
pub(crate) struct Reply {
    prefix: encode::Prefix,
    blocks: Vec<String>,
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("prefix", &self.prefix)
            .field("blocks", &self.blocks.len())
            .finish()
    }
}

impl fmt::Debug for Mapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapper")
            .field("blocks", &self.blocks.len())
            .field("events", &self.events.len())
            .field("bytes", &self.bytes)
            .field("broken", &self.broken)
            .field("terminal", &self.terminal)
            .field("answering", &self.answering)
            .field("kept", &self.kept.len())
            .finish_non_exhaustive()
    }
}

impl Mapper {
    /// Maps a callback without allowing later input to undo a failure or ending.
    pub(crate) fn event(&mut self, event: &SseEvent) {
        if self.terminal || (self.broken && event.kind() != "message_delta") {
            return;
        }
        if self.map(event).is_err() {
            self.broken = true;
            if event.kind() == "message_delta" {
                self.final_usage = false;
            }
        }
    }

    /// The round's whole reply, once: only after this mapper's request completed with every block
    /// representable and none missing, never after a failure or a partial stream.
    pub(crate) fn reply(&mut self) -> Option<Reply> {
        if !self.terminal {
            return None;
        }
        let prefix = self.answering.take()?;
        let mut kept = std::mem::take(&mut self.kept);
        kept.sort_by_key(|(index, _)| *index);
        self.bytes -= kept.iter().map(|(_, block)| block.len()).sum::<usize>();
        if (0..).zip(&kept).any(|(at, (index, _))| at != *index) {
            return None;
        }
        Some(Reply {
            prefix,
            blocks: kept.into_iter().map(|(_, block)| block).collect(),
        })
    }

    /// Transfers queued ownership to the caller, releasing its byte charge.
    pub(crate) fn take(&mut self) -> Vec<TurnEvent> {
        let events = std::mem::take(&mut self.events);
        self.bytes -= events.iter().map(event_bytes).sum::<usize>();
        events
    }

    /// Stages exact final usage, then returns the ending once. Drain `take` before Finished.
    /// Premature EOF never publishes a potentially incomplete usage snapshot.
    /// Message start reserves Usage's overhead so a full queue cannot hide a semantic failure.
    pub(crate) fn finish(&mut self, read: Result<Progress, Failure>) -> Option<Finish> {
        if self.terminal {
            return None;
        }
        let finish = match read {
            Err(failure @ (Failure::Exhausted | Failure::Refused | Failure::Provider(_))) => {
                Finish::Failed(EngineFailure::Anthropic(failure))
            }
            _ if self.broken => Finish::Failed(EngineFailure::Anthropic(Failure::Protocol)),
            Ok(Progress::More) => return None,
            Ok(Progress::Completed)
                if self.usage.is_none()
                    || self.blocks.iter().any(|(_, b)| !matches!(b, Block::Closed)) =>
            {
                Finish::Failed(EngineFailure::Anthropic(Failure::Protocol))
            }
            Ok(Progress::Completed) => Finish::Completed,
            Err(failure) => Finish::Failed(EngineFailure::Anthropic(failure)),
        };
        if !matches!(finish, Finish::Completed) {
            self.unbind();
        }
        let exact = matches!(
            &finish,
            Finish::Completed
                | Finish::Failed(EngineFailure::Anthropic(
                    Failure::Exhausted | Failure::Refused
                ))
        );
        if exact
            && self.final_usage
            && let Some(whole) = self.usage
        {
            self.events.push(TurnEvent::Usage(Usage {
                whole,
                first_request: Some(whole),
            }));
        }
        self.terminal = true;
        Some(finish)
    }

    fn map(&mut self, event: &SseEvent) -> Result<(), ()> {
        let v = event.value();
        match event.kind() {
            "message_start" => {
                if self.usage.is_some() || !self.blocks.is_empty() {
                    return Err(());
                }
                self.charge(EVENT_BYTES)?;
                self.usage = Some(Tokens::default());
                self.counts(&v["message"]["usage"])?;
            }
            "message_delta" => {
                if self.usage.is_none() || v.get("delta").is_some_and(|delta| !delta.is_object()) {
                    return Err(());
                }
                if let Some(usage) = v.get("usage") {
                    self.counts(usage)?;
                }
                self.final_usage |= v["delta"]["stop_reason"].is_string();
            }
            "content_block_start" => self.start(index(v)?, &v["content_block"])?,
            "content_block_delta" => self.delta(index(v)?, &v["delta"])?,
            "content_block_stop" => self.stop(index(v)?)?,
            _ => {}
        }
        Ok(())
    }

    fn counts(&mut self, v: &Value) -> Result<(), ()> {
        if !v.is_object() {
            return Err(());
        }
        let mut tokens = self.usage.ok_or(())?;
        for (key, count) in [
            ("input_tokens", &mut tokens.input),
            ("output_tokens", &mut tokens.output),
            ("cache_read_input_tokens", &mut tokens.cache_read),
            ("cache_creation_input_tokens", &mut tokens.cache_write),
        ] {
            if let Some(value) = v.get(key) {
                if value.is_null() && key != "output_tokens" {
                    continue;
                }
                let next = value
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(())?;
                if next < *count {
                    return Err(());
                }
                *count = next;
            }
        }
        self.usage = Some(tokens);
        Ok(())
    }

    fn start(&mut self, index: u32, v: &Value) -> Result<(), ()> {
        if self.usage.is_none()
            || self.blocks.len() == ITEMS
            || self.blocks.iter().any(|(i, _)| *i == index)
        {
            return Err(());
        }
        let block = match text(&v["type"])? {
            "text" => {
                let initial = text(&v["text"])?;
                let kept = if self.answering.is_some() {
                    self.charge(initial.len())?;
                    initial.to_owned()
                } else {
                    String::new()
                };
                if !initial.is_empty() {
                    self.charge(initial.len() + EVENT_BYTES)?;
                    self.events.push(TurnEvent::TextDelta(initial.to_owned()));
                }
                Block::Text(kept)
            }
            "tool_use" => {
                let id = text(&v["id"])?;
                let name = text(&v["name"])?;
                if name.is_empty() || name.len() > NAME || !v["input"].is_object() {
                    return Err(());
                }
                let call = CallId::new(id).map_err(|_| ())?;
                if self.ids.contains(&call) {
                    return Err(());
                }
                self.charge(id.len() * 2)?;
                let name = if self.answering.is_some() {
                    self.charge(name.len())?;
                    name.to_owned()
                } else {
                    String::new()
                };
                let start = self.serialize(&v["input"], BYTES)?;
                self.charge(start.len())?;
                self.queue(TurnEvent::ToolCallStarted {
                    call: call.clone(),
                    name: ToolName::from_wire(text(&v["name"])?),
                })?;
                self.ids.push(call.clone());
                Block::Tool {
                    call,
                    name,
                    start: String::from_utf8(start).map_err(|_| ())?,
                    args: None,
                }
            }
            "thinking" => {
                let thought = text(&v["thinking"])?;
                let signature = v
                    .get("signature")
                    .map(text)
                    .transpose()?
                    .unwrap_or_default();
                self.charge(thought.len().checked_add(signature.len()).ok_or(())?)?;
                Block::Thinking {
                    text: thought.to_owned(),
                    signature: signature.to_owned(),
                }
            }
            "redacted_thinking" => {
                let data = text(&v["data"])?;
                self.charge(data.len())?;
                Block::RedactedThinking(data.to_owned())
            }
            _ => Block::Skipped,
        };
        self.blocks.push((index, block));
        Ok(())
    }

    fn delta(&mut self, index: u32, v: &Value) -> Result<(), ()> {
        let at = self.at(index)?;
        let kind = text(&v["type"])?;
        let (key, copies) = match (&self.blocks[at].1, kind) {
            (Block::Text(_), "text_delta") => ("text", 1 + usize::from(self.answering.is_some())),
            (Block::Tool { .. }, "input_json_delta") => ("partial_json", 2),
            (Block::Thinking { .. }, "thinking_delta") => ("thinking", 1),
            (Block::Thinking { .. }, "signature_delta") => ("signature", 1),
            (Block::Skipped, _) => return Ok(()),
            _ => return Err(()),
        };
        let delta = text(&v[key])?;
        if kind == "input_json_delta" && delta.is_empty() {
            return Ok(());
        }
        let queued = match &self.blocks[at].1 {
            Block::Tool { call, .. } => call.as_str().len() + EVENT_BYTES,
            Block::Text(_) => EVENT_BYTES,
            _ => 0,
        };
        self.charge(
            delta
                .len()
                .checked_mul(copies)
                .and_then(|n| n.checked_add(queued))
                .ok_or(())?,
        )?;
        match &mut self.blocks[at].1 {
            Block::Text(kept) => {
                if self.answering.is_some() {
                    kept.push_str(delta);
                }
                self.events.push(TurnEvent::TextDelta(delta.to_owned()));
            }
            Block::Tool {
                call, start, args, ..
            } => {
                if args.is_none() {
                    self.bytes -= start.len();
                    *start = String::new();
                }
                args.get_or_insert_with(String::new).push_str(delta);
                self.events.push(TurnEvent::ToolArgsDelta {
                    call: call.clone(),
                    delta: delta.to_owned(),
                });
            }
            Block::Thinking { text, signature } => {
                if kind == "thinking_delta" {
                    text.push_str(delta);
                } else {
                    signature.push_str(delta);
                }
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn stop(&mut self, index: u32) -> Result<(), ()> {
        let at = self.at(index)?;
        let old = std::mem::replace(&mut self.blocks[at].1, Block::Closed);
        let retained = match &old {
            Block::Tool {
                call,
                name,
                start,
                args,
            } => {
                call.as_str().len()
                    + name.len()
                    + start.len()
                    + args.as_ref().map_or(0, String::len)
            }
            Block::Text(text) => text.len(),
            Block::Thinking { text, signature } => text.len() + signature.len(),
            Block::RedactedThinking(data) => data.len(),
            _ => 0,
        };
        let kept = match old {
            Block::Tool {
                call,
                name,
                start,
                args,
            } => {
                let initial = args.is_none() && start != "{}";
                let args = args.unwrap_or(start);
                if !serde_json::from_str::<Value>(&args)
                    .map_err(|_| ())?
                    .is_object()
                {
                    return Err(());
                }
                self.bytes -= retained;
                if initial {
                    // A start object came through a parse that can round numbers; fragments are raw.
                    self.unbind();
                    self.queue(TurnEvent::ToolArgsDelta {
                        call: call.clone(),
                        delta: args.clone(),
                    })?;
                }
                let kept = self.answering.is_some().then(|| {
                    format!(
                        r#"{{"type":"tool_use","id":{},"name":{},"input":{args}}}"#,
                        Value::from(call.as_str()),
                        Value::from(name),
                    )
                });
                self.queue(TurnEvent::ToolCallReady { call, args })?;
                return self.keep(index, kept);
            }
            Block::Thinking { text, signature } => {
                if signature.is_empty() {
                    return Err(());
                }
                let item = Native::Thinking {
                    thinking: &text,
                    signature: &signature,
                };
                self.native(index, &item)?
            }
            Block::RedactedThinking(data) => {
                self.native(index, &Native::RedactedThinking { data: &data })?
            }
            Block::Text(text) => {
                // Messages refuses an empty text block, so one can't go back as it came.
                if text.is_empty() {
                    self.unbind();
                }
                (!text.is_empty())
                    .then(|| format!(r#"{{"type":"text","text":{}}}"#, Value::from(text)))
            }
            Block::Skipped => {
                self.unbind();
                None
            }
            Block::Closed => return Err(()),
        };
        self.bytes -= retained;
        self.keep(index, kept)
    }

    /// Queues a thinking item, returning its JSON for a bound mapper's reply.
    fn native(&mut self, index: u32, item: &Native<'_>) -> Result<Option<String>, ()> {
        let bytes = self.serialize(item, Opaque::MAX)?;
        let kept = self
            .answering
            .is_some()
            .then(|| String::from_utf8(bytes.clone()))
            .transpose()
            .map_err(|_| ())?;
        self.queue(TurnEvent::OutputItem {
            index,
            item: Opaque::new(bytes).map_err(|_| ())?,
        })?;
        Ok(kept)
    }

    /// Drops the reply when it can't go back as it came, so this stream continues nothing.
    fn unbind(&mut self) {
        self.answering = None;
        self.bytes -= self
            .kept
            .iter()
            .map(|(_, block)| block.len())
            .sum::<usize>();
        self.kept.clear();
    }

    /// Keeps a stopped block's JSON for a bound mapper's reply, charged until `reply` takes it.
    fn keep(&mut self, index: u32, block: Option<String>) -> Result<(), ()> {
        if let Some(block) = block.filter(|_| self.answering.is_some()) {
            self.charge(block.len())?;
            self.kept.push((index, block));
        }
        Ok(())
    }

    fn at(&self, index: u32) -> Result<usize, ()> {
        self.blocks
            .iter()
            .position(|(i, b)| *i == index && !matches!(b, Block::Closed))
            .ok_or(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), ()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= BYTES)
            .ok_or(())?;
        Ok(())
    }

    fn queue(&mut self, event: TurnEvent) -> Result<(), ()> {
        self.charge(event_bytes(&event))?;
        self.events.push(event);
        Ok(())
    }

    fn serialize(&self, value: &impl serde::Serialize, cap: usize) -> Result<Vec<u8>, ()> {
        let mut writer = Bounded {
            bytes: Vec::new(),
            cap: cap.min(BYTES - self.bytes),
        };
        serde_json::to_writer(&mut writer, value).map_err(|_| ())?;
        Ok(writer.bytes)
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Native<'a> {
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    RedactedThinking {
        data: &'a str,
    },
}

impl fmt::Debug for Native<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Thinking { .. } => "Thinking",
            Self::RedactedThinking { .. } => "RedactedThinking",
        })
    }
}

struct Bounded {
    bytes: Vec<u8>,
    cap: usize,
}

impl fmt::Debug for Bounded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bounded")
            .field("bytes", &self.bytes.len())
            .field("cap", &self.cap)
            .finish()
    }
}

impl io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.cap - self.bytes.len() {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn text(value: &Value) -> Result<&str, ()> {
    value.as_str().ok_or(())
}
fn index(value: &Value) -> Result<u32, ()> {
    value["index"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(())
}
fn event_bytes(event: &TurnEvent) -> usize {
    EVENT_BYTES
        + match event {
            TurnEvent::TextDelta(text) => text.len(),
            TurnEvent::ToolCallStarted { call, .. } => call.as_str().len(),
            TurnEvent::ToolArgsDelta { call, delta } => call.as_str().len() + delta.len(),
            TurnEvent::ToolCallReady { call, args } => call.as_str().len() + args.len(),
            TurnEvent::OutputItem { item, .. } => item.size(),
            _ => 0,
        }
}

#[cfg(test)]
#[path = "anthropic/tests.rs"]
mod tests;
