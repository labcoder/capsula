//! The small Messages envelope checked before admission; A3 owns content encoding.

use std::fmt;
use std::io::{self, Write};

use kumochi_protocol::{Secret, message::ModelSlug};
use serde_json::Value;
use zeroize::Zeroizing;

use super::error::Failure;
use crate::connections::model::MAX_REQUEST_BYTES;

/// Serialized, validated Messages input, without a content-bearing formatter.
pub struct MessagesBody(Secret<Zeroizing<Vec<u8>>>);

impl MessagesBody {
    /// Checks the supported envelope and its one stable-prefix cache breakpoint.
    ///
    /// # Errors
    /// [`Failure::Request`] for an invalid envelope or more than 16 MiB of JSON.
    pub fn new(request: &Value) -> Result<Self, Failure> {
        check(request)?;
        let bytes = encode(request, MAX_REQUEST_BYTES).map_err(|()| Failure::Request)?;
        Ok(Self(Secret::new(bytes)))
    }

    /// Checks serialized JSON as [`MessagesBody::new`] checks a value, then keeps these exact
    /// bytes, so a number in them is sent as it was written.
    ///
    /// # Errors
    /// [`Failure::Request`] for JSON that isn't a valid envelope or is over 16 MiB.
    pub fn from_json(bytes: Zeroizing<Vec<u8>>) -> Result<Self, Failure> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Failure::Request);
        }
        check(&serde_json::from_slice(&bytes).map_err(|_| Failure::Request)?)?;
        Ok(Self(Secret::new(bytes)))
    }

    pub(super) fn bytes(&self) -> &[u8] {
        self.0.expose()
    }
}

fn check(request: &Value) -> Result<(), Failure> {
    let object = request.as_object().ok_or(Failure::Request)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "model" | "messages" | "max_tokens" | "stream" | "system" | "tools" | "output_config"
        )
    }) || request["stream"] != true
        || request["max_tokens"].as_u64() != Some(2048)
    {
        return Err(Failure::Request);
    }
    ModelSlug::new(request["model"].as_str().ok_or(Failure::Request)?)
        .map_err(|_| Failure::Request)?;
    let messages = request["messages"].as_array().ok_or(Failure::Request)?;
    if messages.is_empty()
        || messages.iter().any(|message| {
            !matches!(message["role"].as_str(), Some("user" | "assistant"))
                || message.get("cache_control").is_some()
                || !content(&message["content"])
        })
    {
        return Err(Failure::Request);
    }
    if let Some(config) = object.get("output_config") {
        let config = config.as_object().ok_or(Failure::Request)?;
        if config.len() != 1
            || !matches!(
                config.get("effort").and_then(Value::as_str),
                Some("low" | "medium" | "high" | "xhigh" | "max")
            )
        {
            return Err(Failure::Request);
        }
    }
    let system = object
        .get("system")
        .map(|value| value.as_array().ok_or(Failure::Request))
        .transpose()?;
    let tools = object
        .get("tools")
        .map(|value| value.as_array().ok_or(Failure::Request))
        .transpose()?;
    if tools.is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.get("type").is_some_and(|kind| kind != "custom"))
    }) {
        return Err(Failure::Request);
    }
    if system.is_some_and(Vec::is_empty) {
        return Err(Failure::Request);
    }
    let mut breakpoints = 0;
    for (blocks, stable_end) in [(tools, system.is_none()), (system, true)] {
        if let Some(blocks) = blocks {
            for (index, block) in blocks.iter().enumerate() {
                let block = block.as_object().ok_or(Failure::Request)?;
                if let Some(cache) = block.get("cache_control") {
                    if !stable_end
                        || index + 1 != blocks.len()
                        || cache != &serde_json::json!({"type":"ephemeral"})
                    {
                        return Err(Failure::Request);
                    }
                    breakpoints += 1;
                }
            }
        }
    }
    if breakpoints != 1 {
        return Err(Failure::Request);
    }
    Ok(())
}

pub(super) fn encode(value: &Value, limit: usize) -> Result<Zeroizing<Vec<u8>>, ()> {
    struct Counter {
        length: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.length {
                return Err(io::Error::other("JSON exceeds its byte bound"));
            }
            self.length += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { length: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| ())?;
    let mut bytes = Zeroizing::new(vec![0; counter.length]);
    let mut output = bytes.as_mut_slice();
    serde_json::to_writer(&mut output, value).map_err(|_| ())?;
    if !output.is_empty() {
        return Err(());
    }
    Ok(bytes)
}

fn content(value: &Value) -> bool {
    value.is_string()
        || value.as_array().is_some_and(|blocks| {
            blocks.iter().all(|block| {
                block.is_object()
                    && block["type"].as_str().is_some_and(|kind| !kind.is_empty())
                    && block.get("cache_control").is_none()
                    && block.get("content").is_none_or(content)
            })
        })
}

impl fmt::Debug for MessagesBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagesBody")
            .field("bytes", &self.bytes().len())
            .finish()
    }
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
