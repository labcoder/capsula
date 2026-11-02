//! Model ids, host identities and millisecond times.

use crate::{
    error::ErrorCode,
    limits::{MAX_ID_CHARS, MAX_MOTION_MS},
};
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

/// A relative time in milliseconds, bounded to one minute.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Ms(u32);

impl Ms {
    /// Returns the relative time in milliseconds.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Adds relative times, returning none if the result exceeds the motion cap.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0
            .checked_add(other.0)
            .and_then(|value| Self::try_from(value).ok())
    }
}

impl TryFrom<u32> for Ms {
    type Error = ErrorCode;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value > MAX_MOTION_MS {
            return Err(ErrorCode::OutOfRange);
        }
        Ok(Self(value))
    }
}

impl From<Ms> for u32 {
    fn from(value: Ms) -> Self {
        value.0
    }
}
