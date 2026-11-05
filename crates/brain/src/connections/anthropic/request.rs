//! The small Messages envelope checked before admission; A3 owns content encoding.

use std::fmt;

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
        let object = request.as_object().ok_or(Failure::Request)?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "model"
                    | "messages"
                    | "max_tokens"
                    | "stream"
                    | "system"
                    | "tools"
                    | "output_config"
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
        let bytes = Zeroizing::new(serde_json::to_vec(request).map_err(|_| Failure::Request)?);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Failure::Request);
        }
        Ok(Self(Secret::new(bytes)))
    }

    pub(super) fn bytes(&self) -> &[u8] {
        self.0.expose()
    }
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
