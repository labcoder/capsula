//! The Responses event stream: the reader that reads the shared framer's events as Responses
//! events and ends the stream on its terminal event. It never touches the network; the transport
//! feeds it bytes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::ControlFlow;

use serde_json::Value;

use super::error::{Failure, ProviderError};
use super::framer::{self, Framer, Limit};

/// The longest line, its line feed excluded.
pub const MAX_LINE_BYTES: usize = framer::MAX_LINE_BYTES;
/// The most data one event carries, its `data` lines joined by line feeds.
pub const MAX_EVENT_BYTES: usize = framer::MAX_EVENT_BYTES;
/// The most bytes one response stream carries.
pub const MAX_STREAM_BYTES: usize = framer::MAX_STREAM_BYTES;
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
    /// The event for `value`, when it's an object with a string `type`.
    pub(in crate::connections) fn from_value(value: Value) -> Option<Self> {
        value["type"].is_string().then_some(Self(value))
    }

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

impl From<Limit> for Failure {
    fn from(_: Limit) -> Self {
        Self::Protocol
    }
}

/// Reads one response stream. Only `response.completed` succeeds; a failed, incomplete or
/// error event, a cap passed, or bytes that aren't events end it with a [`Failure`]. A stream
/// whose bytes end first is [`Failure::Truncated`].
#[derive(Default)]
pub struct Reader {
    framer: Framer,
    text: usize,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("received", &self.framer.received())
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
        bytes: &[u8],
        on_event: &mut impl FnMut(&SseEvent),
    ) -> Result<Progress, Failure> {
        let text = &mut self.text;
        let flow = self
            .framer
            .feed(bytes, &mut |data| dispatch(data, text, on_event))?;
        Ok(if flow.is_break() {
            Progress::Completed
        } else {
            Progress::More
        })
    }
}

/// Reads one event's data: `[DONE]` is skipped, and anything else must be a typed JSON event.
fn dispatch(
    data: &[u8],
    text: &mut usize,
    on_event: &mut impl FnMut(&SseEvent),
) -> Result<ControlFlow<()>, Failure> {
    if data == b"[DONE]" {
        return Ok(ControlFlow::Continue(()));
    }
    let event = serde_json::from_slice::<Value>(data)
        .ok()
        .and_then(SseEvent::from_value)
        .ok_or(Failure::Protocol)?;
    match event.kind() {
        "response.output_text.delta" => {
            let delta = event.0["delta"].as_str().ok_or(Failure::Protocol)?;
            *text += delta.len();
            if *text > MAX_TEXT_BYTES {
                return Err(Failure::Protocol);
            }
        }
        "response.completed" => {
            on_event(&event);
            return Ok(ControlFlow::Break(()));
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
    Ok(ControlFlow::Continue(()))
}
