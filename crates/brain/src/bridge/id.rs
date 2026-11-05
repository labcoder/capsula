//! JSON-RPC ids as the client sent them, and the call key the brain sees instead.

use std::fmt::{self, Write as _};

use serde_json::Value;

use super::wire::CallKey;

/// One Claude Code launch, drawn when the launch starts; its hex names the launch's pipe.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LaunchId(pub(crate) u128);

/// Only that it is one: a launch id names a pipe.
impl fmt::Debug for LaunchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchId")
    }
}

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
