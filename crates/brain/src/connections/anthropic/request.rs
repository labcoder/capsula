//! The small Messages envelope checked before admission; A3 owns content encoding.

use std::fmt;
use std::io::{self, Write};

use kumochi_protocol::{Secret, message::ModelSlug};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
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

    /// Checks serialized JSON as [`MessagesBody::new`] checks a value, read by [`literal`], then
    /// keeps these exact bytes, so a number in them is sent as it was written. Only the Messages
    /// encoder sends a body it wrote this way.
    pub(crate) fn from_json(bytes: Zeroizing<Vec<u8>>) -> Result<Self, Failure> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Failure::Request);
        }
        check(&literal(&bytes)?)?;
        Ok(Self(Secret::new(bytes)))
    }

    /// The bytes sent, for a test outside the transport.
    #[cfg(test)]
    pub(crate) fn sent(&self) -> &[u8] {
        self.bytes()
    }

    pub(super) fn bytes(&self) -> &[u8] {
        self.0.expose()
    }
}

/// `bytes` as one JSON value read exactly as written: a member name repeated in any object is
/// refused, since readers differ on which copy counts. Every Anthropic reply byte is read this
/// way. Names are read as plain strings, so no object is taken for `serde_json`'s embedded raw
/// value, even in a workspace build, where gpui turns `raw_value` on.
pub(crate) fn literal(bytes: &[u8]) -> Result<Value, Failure> {
    serde_json::from_slice::<Literal>(bytes)
        .map(|Literal(value)| value)
        .map_err(|_| Failure::Request)
}

/// The members of `object`, JSON that [`literal`] has read, in order: each name decoded and
/// each value exactly as written. None for anything else.
pub(crate) fn members(object: &str) -> Option<Vec<(String, &str)>> {
    let bytes = object.as_bytes();
    let mut at = space(bytes, 0);
    if bytes.get(at) != Some(&b'{') {
        return None;
    }
    at = space(bytes, at + 1);
    let mut members = Vec::new();
    if bytes.get(at) == Some(&b'}') {
        return (space(bytes, at + 1) == bytes.len()).then_some(members);
    }
    loop {
        let named = end(bytes, at)?;
        let name = serde_json::from_str(object.get(at..named)?).ok()?;
        at = space(bytes, named);
        if bytes.get(at) != Some(&b':') {
            return None;
        }
        let start = space(bytes, at + 1);
        let stop = end(bytes, start)?;
        members.push((name, object.get(start..stop)?));
        at = space(bytes, stop);
        match bytes.get(at) {
            Some(b',') => at = space(bytes, at + 1),
            Some(b'}') => return (space(bytes, at + 1) == bytes.len()).then_some(members),
            _ => return None,
        }
    }
}

fn space(bytes: &[u8], mut at: usize) -> usize {
    while bytes
        .get(at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        at += 1;
    }
    at
}

/// Where the value starting at `at` ends, in JSON already read: only strings and brackets
/// need tracking, since a scalar runs to the next delimiter.
fn end(bytes: &[u8], mut at: usize) -> Option<usize> {
    let mut depth = 0_usize;
    loop {
        match *bytes.get(at)? {
            b'"' => {
                at += 1;
                loop {
                    match *bytes.get(at)? {
                        b'\\' => at += 2,
                        b'"' => break,
                        _ => at += 1,
                    }
                }
                at += 1;
            }
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                at += 1;
            }
            _ if depth == 0 => {
                while bytes.get(at).is_some_and(|byte| {
                    !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                }) {
                    at += 1;
                }
                return Some(at);
            }
            _ => at += 1,
        }
        if depth == 0 {
            return Some(at);
        }
    }
}

struct Literal(Value);

impl<'de> Deserialize<'de> for Literal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Literally)
    }
}

struct Literally;

impl<'de> Visitor<'de> for Literally {
    type Value = Literal;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Literal, E> {
        Ok(Literal(value.into()))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Literal, E> {
        Ok(Literal(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Literal, E> {
        Ok(Literal(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Literal, E> {
        Ok(Literal(value.into()))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Literal, E> {
        Ok(Literal(value.into()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Literal, E> {
        Ok(Literal(Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Literal, A::Error> {
        let mut items = Vec::new();
        while let Some(Literal(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Literal(Value::Array(items)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Literal, A::Error> {
        let mut object = Map::new();
        while let Some(name) = map.next_key::<String>()? {
            let Literal(value) = map.next_value()?;
            if object.insert(name, value).is_some() {
                return Err(de::Error::custom("a repeated member name"));
            }
        }
        Ok(Literal(Value::Object(object)))
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
