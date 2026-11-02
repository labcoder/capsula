//! Model ids, host identities and millisecond times.

use crate::{
    error::ErrorCode,
    limits::{MAX_ID_CHARS, MAX_MOTION_MS, MAX_STEP_OPS, MAX_STEPS},
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
#[serde(try_from = "u32", into = "u32")]
pub struct Handle(u32);

impl Handle {
    /// Encodes a one-based commit revision and zero-based expanded put slot.
    /// # Errors
    /// Returns `OutOfRange` for a revision or slot outside scene v1's bounds.
    pub fn new(revision: u32, slot: usize) -> Result<Self, ErrorCode> {
        if !usize::try_from(revision).is_ok_and(|r| (1..=MAX_STEPS).contains(&r))
            || slot >= MAX_STEP_OPS
        {
            return Err(ErrorCode::OutOfRange);
        }
        let slot = u32::try_from(slot).map_err(|_| ErrorCode::OutOfRange)?;
        Ok(Self((revision << 10) | slot))
    }

    /// Returns the stable wire representation.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Returns the original one-based commit revision.
    pub fn revision(self) -> u32 {
        self.0 >> 10
    }

    /// Returns the original zero-based expanded put slot.
    pub fn slot(self) -> usize {
        let [low, high, ..] = self.0.to_le_bytes();
        usize::from(u16::from_le_bytes([low, high & 3]))
    }
}

impl TryFrom<u32> for Handle {
    type Error = ErrorCode;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        let slot = usize::try_from(value & 1023).map_err(|_| ErrorCode::OutOfRange)?;
        Self::new(value >> 10, slot)
    }
}
impl From<Handle> for u32 {
    fn from(value: Handle) -> Self {
        value.0
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
