//! The Responses event stream: a bounded server-sent-events framer, and the reader that ends the
//! stream on its terminal event. It never touches the network; the transport feeds it bytes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::mem;

use serde_json::Value;

use super::error::{Failure, ProviderError};

/// The longest line, its line feed excluded.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// The most data one event carries, its `data` lines joined by line feeds.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// The most bytes one response stream carries.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
/// The most visible text one response streams.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// The event types whose names Debug may print; any other prints as `other`.
const KNOWN: [&str; 16] = [
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.output_item.done",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.completed",
    "response.failed",
    "response.incomplete",
    "error",
];

/// One event's JSON object, which always has a string `type`.
pub struct SseEvent(Value);

impl SseEvent {
    /// The event's `type`.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.0["type"].as_str().unwrap_or_default()
    }

    /// The whole event.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }
}

impl fmt::Debug for SseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.kind();
        let kind = KNOWN.iter().find(|known| **known == kind).copied();
        f.debug_struct("SseEvent")
            .field("kind", &kind.unwrap_or("other"))
            .finish()
    }
}

/// Where a stream stands after some of its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No terminal event yet.
    More,
    /// `response.completed` arrived; the bytes after it are ignored.
    Completed,
}

/// Reads one response stream. Only `response.completed` succeeds; a failed, incomplete or
/// error event, a cap passed, or bytes that aren't events end it with a [`Failure`]. A stream
/// whose bytes end first is [`Failure::Truncated`].
#[derive(Default)]
pub struct Reader {
    /// The line so far, when a chunk ended inside it.
    line: Vec<u8>,
    /// The event's data so far.
    data: Vec<u8>,
    /// The event has a `data` line, possibly empty.
    has_data: bool,
    received: usize,
    text: usize,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("received", &self.received)
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Reads the stream's next bytes, handing each event before the terminal one to
    /// `on_event`, `response.completed` included.
    ///
    /// # Errors
    /// The stream's end when it isn't a success, or a cap it passed.
    pub fn feed(
        &mut self,
        mut bytes: &[u8],
        on_event: &mut impl FnMut(&SseEvent),
    ) -> Result<Progress, Failure> {
        self.received = self
            .received
            .checked_add(bytes.len())
            .filter(|received| *received <= MAX_STREAM_BYTES)
            .ok_or(Failure::Protocol)?;
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            let (head, rest) = bytes.split_at(end);
            bytes = &rest[1..];
            let progress = if self.line.is_empty() {
                self.line_ended(head, on_event)?
            } else {
                let mut line = mem::take(&mut self.line);
                if line.len() + head.len() > MAX_LINE_BYTES {
                    return Err(Failure::Protocol);
                }
                line.extend_from_slice(head);
                let progress = self.line_ended(&line, on_event);
                line.clear();
                self.line = line;
                progress?
            };
            if progress == Progress::Completed {
                return Ok(progress);
            }
        }
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            return Err(Failure::Protocol);
        }
        self.line.extend_from_slice(bytes);
        Ok(Progress::More)
    }

    fn line_ended(
        &mut self,
        line: &[u8],
        on_event: &mut impl FnMut(&SseEvent),
    ) -> Result<Progress, Failure> {
        if line.len() > MAX_LINE_BYTES {
            return Err(Failure::Protocol);
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.dispatch(on_event);
        }
        let (field, value) = match line.iter().position(|&byte| byte == b':') {
            Some(0) => return Ok(Progress::More),
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        // The JSON's own type names the event, so `event`, `id` and `retry` are ignored.
        if field == b"data" {
            let joined = usize::from(self.has_data) + value.len();
            if self.data.len() + joined > MAX_EVENT_BYTES {
                return Err(Failure::Protocol);
            }
            if self.has_data {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            self.has_data = true;
        }
        Ok(Progress::More)
    }

    fn dispatch(&mut self, on_event: &mut impl FnMut(&SseEvent)) -> Result<Progress, Failure> {
        if !mem::take(&mut self.has_data) {
            return Ok(Progress::More);
        }
        let mut data = mem::take(&mut self.data);
        let parsed = (data != b"[DONE]").then(|| serde_json::from_slice::<Value>(&data));
        data.clear();
        self.data = data;
        let Some(value) = parsed else {
            return Ok(Progress::More);
        };
        let event = value
            .ok()
            .filter(|value| value["type"].is_string())
            .map(SseEvent)
            .ok_or(Failure::Protocol)?;
        match event.kind() {
            "response.output_text.delta" => {
                let delta = event.0["delta"].as_str().ok_or(Failure::Protocol)?;
                self.text += delta.len();
                if self.text > MAX_TEXT_BYTES {
                    return Err(Failure::Protocol);
                }
            }
            "response.completed" => {
                on_event(&event);
                return Ok(Progress::Completed);
            }
            "response.failed" => {
                return Err(Failure::Provider(ProviderError::streamed(
                    &event.0["response"]["error"],
                )));
            }
            "response.incomplete" => return Err(Failure::Incomplete),
            "error" => return Err(Failure::Provider(ProviderError::streamed(&event.0))),
            _ => {}
        }
        on_event(&event);
        Ok(Progress::More)
    }
}
