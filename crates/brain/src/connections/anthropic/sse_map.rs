//! Messages stream semantics over the shared bounded SSE framing, without tool assembly.

use std::fmt;
use std::ops::ControlFlow;

use crate::connections::model::framer::{Framer, Limit};
use crate::connections::model::sse::{MAX_TEXT_BYTES, Progress, SseEvent};

use super::error::Failure;
use super::request::literal;

/// A successful round ending; tool execution belongs to the engine, not this reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The assistant finished its turn.
    EndTurn,
    /// The assistant requested tools.
    ToolUse,
    /// A requested stop sequence ended the round.
    StopSequence,
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Open,
    Completed(StopReason),
    Failed(Failure),
}

/// What a reader keeps of the events it reads, beside their parsed JSON.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Nothing: an unbound stream.
    #[default]
    Nothing,
    /// Each block start's bytes, for a mapper bound to its request.
    BlockStarts,
}

/// One sticky stream: only `message_stop` after a supported stop reason completes it.
#[derive(Default)]
pub struct Reader {
    /// Whether block starts keep their bytes, for a reply that goes back as it came.
    capture: Capture,
    framer: Framer,
    text: usize,
    reason: Option<StopReason>,
    state: State,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicReader")
            .field("capture", &self.capture)
            .field("text_bytes", &self.text)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl From<Limit> for Failure {
    fn from(_: Limit) -> Self {
        Self::Protocol
    }
}

impl Reader {
    /// A fresh reader that keeps what `capture` names.
    #[must_use]
    pub fn new(capture: Capture) -> Self {
        Self {
            capture,
            ..Self::default()
        }
    }

    /// Forward valid known events in order, ignoring pings and bounded future event types.
    ///
    /// # Errors
    /// A closed protocol, provider or truncation failure, sticky on every later call.
    ///
    /// # Panics
    /// If the consumer callback panics or the checked terminal-reason invariant breaks.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        on_event: &mut impl FnMut(&SseEvent),
    ) -> Result<Progress, Failure> {
        match &self.state {
            State::Completed(_) => return Ok(Progress::Completed),
            State::Failed(failure) => return Err(failure.clone()),
            State::Open => {}
        }
        let result = self.framer.feed(bytes, &mut |data| {
            let value = literal(data).map_err(|_| Failure::Protocol)?;
            let mut event = SseEvent::from_value(value).ok_or(Failure::Protocol)?;
            match event.kind() {
                "error" => return Err(Failure::streamed(event.value())),
                "content_block_delta" => {
                    let delta = &event.value()["delta"];
                    let kind = delta["type"].as_str().ok_or(Failure::Protocol)?;
                    if kind == "text_delta" {
                        let text = delta["text"].as_str().ok_or(Failure::Protocol)?;
                        self.text = self
                            .text
                            .checked_add(text.len())
                            .filter(|count| *count <= MAX_TEXT_BYTES)
                            .ok_or(Failure::Protocol)?;
                    }
                }
                "message_delta" => {
                    if event
                        .value()
                        .get("delta")
                        .is_some_and(|delta| !delta.is_object())
                    {
                        return Err(Failure::Protocol);
                    }
                    let reason = &event.value()["delta"]["stop_reason"];
                    if !reason.is_null() {
                        self.reason = Some(match reason.as_str() {
                            Some("end_turn") => StopReason::EndTurn,
                            Some("tool_use") => StopReason::ToolUse,
                            Some("stop_sequence") => StopReason::StopSequence,
                            Some("max_tokens" | "model_context_window_exceeded") => {
                                on_event(&event);
                                return Err(Failure::Exhausted);
                            }
                            Some("refusal") => {
                                on_event(&event);
                                return Err(Failure::Refused);
                            }
                            _ => return Err(Failure::Protocol),
                        });
                    }
                }
                "message_stop" => {
                    self.reason.ok_or(Failure::Protocol)?;
                    on_event(&event);
                    return Ok(ControlFlow::Break(()));
                }
                "content_block_start" => {
                    if self.capture == Capture::BlockStarts {
                        event.capture(data);
                    }
                }
                "message_start" | "content_block_stop" => {}
                _ => return Ok(ControlFlow::Continue(())),
            }
            on_event(&event);
            Ok(ControlFlow::Continue(()))
        });
        match result {
            Ok(ControlFlow::Continue(())) => Ok(Progress::More),
            Ok(ControlFlow::Break(())) => {
                self.state = State::Completed(
                    self.reason
                        .expect("message_stop requires a checked stop reason"),
                );
                Ok(Progress::Completed)
            }
            Err(failure) => {
                self.state = State::Failed(failure.clone());
                Err(failure)
            }
        }
    }

    /// End the stream, refusing EOF before its terminal event even after visible output.
    ///
    /// # Errors
    /// The sticky failure, or Truncated if no `message_stop` arrived.
    pub fn finish(&mut self) -> Result<StopReason, Failure> {
        match &self.state {
            State::Completed(reason) => Ok(*reason),
            State::Failed(failure) => Err(failure.clone()),
            State::Open => {
                self.state = State::Failed(Failure::Truncated);
                Err(Failure::Truncated)
            }
        }
    }
}

#[cfg(test)]
mod tests;
