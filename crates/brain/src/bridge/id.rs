//! JSON-RPC ids as the client sent them, the call key the brain sees instead, and the launch
//! a bridge serves.

use std::fmt::{self, Write as _};

use serde_json::Value;

use super::wire::CallKey;

/// The longest string id, in bytes: `m:s:` and two hex digits a byte fit `CallId`'s 64 bytes.
pub(crate) const MAX_STRING_ID: usize = 29;

/// A JSON-RPC id, written back by value.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum JsonRpcId {
    Number(i64),
    String(String),
}

impl JsonRpcId {
    /// The id `value` holds, or `None` for null, a fraction, a number outside `i64`, an object,
    /// a bool, an array or a string over [`MAX_STRING_ID`] bytes.
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number.as_i64().map(Self::Number),
            Value::String(text) if text.len() <= MAX_STRING_ID => Some(Self::String(text.clone())),
            _ => None,
        }
    }

    /// `m:n:<decimal>` or `m:s:<lowercase hex>`, so `1` and `"1"` never collide.
    pub(crate) fn key(&self) -> CallKey {
        CallKey(match self {
            Self::Number(number) => format!("m:n:{number}"),
            Self::String(text) => text.bytes().fold("m:s:".to_owned(), |mut key, byte| {
                write!(key, "{byte:02x}").expect("a String takes writes");
                key
            }),
        })
    }

    /// The id as JSON.
    pub(crate) fn json(&self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::String(text) => Value::String(text.clone()).to_string(),
        }
    }
}

/// One Claude Code launch, drawn at random by C5a. Its 32 hex digits name the launch's pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LaunchId(pub(crate) u128);

impl LaunchId {
    /// The 32 lowercase hex digits the bridge's argv and the pipe's name carry.
    pub(crate) fn hex(self) -> String {
        format!("{:032x}", self.0)
    }
}

/// Whether `key` is one [`JsonRpcId::key`] can produce: `m:n:<i64>` in canonical decimal, or
/// `m:s:` and at most [`MAX_STRING_ID`] bytes in lowercase hex.
pub(crate) fn is_call_key(key: &CallKey) -> bool {
    if let Some(number) = key.0.strip_prefix("m:n:") {
        return number.parse::<i64>().is_ok_and(|n| n.to_string() == number);
    }
    key.0.strip_prefix("m:s:").is_some_and(|hex| {
        hex.len() % 2 == 0
            && hex.len() <= 2 * MAX_STRING_ID
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// The kind and length only: an id is the client's text.
impl fmt::Debug for JsonRpcId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(_) => f.write_str("Number"),
            Self::String(text) => f
                .debug_struct("String")
                .field("bytes", &text.len())
                .finish(),
        }
    }
}
