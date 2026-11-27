//! The Chat Completions event stream, read over the shared bounded SSE framer. A text answer
//! completes only on one `stop`, then `[DONE]`, then the end of the body with nothing after.

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::ControlFlow;

use serde_json::Value;

use super::Failure;
use crate::connections::anthropic::request::literal;
use crate::connections::model::framer::{Framer, Limit};
use crate::connections::model::sse::MAX_TEXT_BYTES;

impl From<Limit> for Failure {
    fn from(_: Limit) -> Self {
        Self::Protocol
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Open,
    /// `finish_reason: "stop"` arrived.
    Stopped,
    /// `[DONE]` followed it; nothing more may come.
    Done,
    Failed(Failure),
}

/// One stream. A failure is final: later bytes can't undo it.
#[derive(Default)]
pub(super) struct Reader {
    framer: Framer,
    text: usize,
    state: State,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalReader")
            .field("text_bytes", &self.text)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Reads the stream's next bytes, handing each piece of text to `on_text` in order.
    ///
    /// # Errors
    /// The first failure, for this call and every later one.
    pub(super) fn feed(&mut self, bytes: &[u8], on_text: &mut impl FnMut(&str)) -> Result<(), Failure> {
        if let State::Failed(failure) = self.state {
            return Err(failure);
        }
        let fed = self
            .framer
            .feed(bytes, &mut |data| event(&mut self.state, &mut self.text, data, on_text));
        if let Err(failure) = fed {
            self.state = State::Failed(failure);
            return Err(failure);
        }
        Ok(())
    }

    /// The stream's end, once its body has ended.
    ///
    /// # Errors
    /// [`Failure::Truncated`] unless `[DONE]` followed `stop`, or the failure already seen.
    pub(super) fn finish(&self) -> Result<(), Failure> {
        match self.state {
            State::Done => Ok(()),
            State::Failed(failure) => Err(failure),
            State::Open | State::Stopped => Err(Failure::Truncated),
        }
    }
}

/// Reads one event's data. Each chunk is JSON with no repeated member, holding one choice at
/// index 0 whose delta carries only text.
fn event(
    state: &mut State,
    text: &mut usize,
    data: &[u8],
    on_text: &mut impl FnMut(&str),
) -> Result<ControlFlow<()>, Failure> {
    if *state == State::Done {
        return Err(Failure::Protocol);
    }
    if data == b"[DONE]" {
        if *state != State::Stopped {
            return Err(Failure::Protocol);
        }
        *state = State::Done;
        return Ok(ControlFlow::Continue(()));
    }
    let chunk = literal(data).map_err(|_| Failure::Protocol)?;
    if chunk.get("error").is_some() {
        return Err(Failure::Provider);
    }
    let [choice] = chunk["choices"].as_array().map(Vec::as_slice).unwrap_or_default() else {
        return Err(Failure::Protocol);
    };
    let delta = choice["delta"].as_object().ok_or(Failure::Protocol)?;
    if choice["index"].as_u64() != Some(0)
        || ["tool_calls", "function_call"]
            .iter()
            .any(|key| delta.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(Failure::Protocol);
    }
    match delta.get("content").unwrap_or(&Value::Null) {
        Value::Null => {}
        Value::String(content) if content.is_empty() => {}
        Value::String(_) if *state == State::Stopped => return Err(Failure::Protocol),
        Value::String(content) => {
            *text += content.len();
            if *text > MAX_TEXT_BYTES {
                return Err(Failure::Protocol);
            }
            on_text(content);
        }
        _ => return Err(Failure::Protocol),
    }
    match &choice["finish_reason"] {
        Value::Null => {}
        Value::String(reason) if reason == "stop" && *state == State::Open => {
            *state = State::Stopped;
        }
        Value::String(reason) if reason == "length" => return Err(Failure::Incomplete),
        _ => return Err(Failure::Protocol),
    }
    Ok(ControlFlow::Continue(()))
}
