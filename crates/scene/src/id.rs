//! Model ids, host identities and millisecond times.

use crate::{error::ErrorCode, limits::MAX_ID_CHARS};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A validated model id, independent of the host handle namespace.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WireId(Box<str>);

impl WireId {
    /// Returns the validated id, for model replies and reference resolution only.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WireId {
    type Error = ErrorCode;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ErrorCode::OutOfRange);
        }
        if value.len() > MAX_ID_CHARS {
            return Err(ErrorCode::TooLong);
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(ErrorCode::OutOfRange);
        }
        Ok(Self(value.into_boxed_str()))
    }
}
impl From<WireId> for String {
    fn from(value: WireId) -> Self {
        value.0.into()
    }
}
impl fmt::Debug for WireId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireId")
            .field("len", &self.0.len())
            .finish()
    }
}

/// A stable host identity, never a model-chosen id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Handle(u32);

impl Handle {
    /// Returns the stable wire representation.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Milliseconds on the wire.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Ms(pub u32);
