//! Validated text, numbers and bounded sequences.

use crate::{
    error::ErrorCode,
    limits::{MAX_COORD, MAX_TEXT_CHARS},
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, SeqAccess, Visitor},
};
use std::{fmt, ops::Deref};

/// Text bounded in Unicode scalar values, never silently truncated.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Text(Box<str>);
impl Text {
    /// Returns drawing content; do not log it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Text {
    type Error = ErrorCode;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.chars().count() > MAX_TEXT_CHARS {
            return Err(ErrorCode::TooLong);
        }
        Ok(Self(value.into_boxed_str()))
    }
}
impl From<Text> for String {
    fn from(value: Text) -> Self {
        value.0.into()
    }
}
impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Text")
            .field("len", &self.0.chars().count())
            .finish()
    }
}

/// A finite canvas number within the coordinate cap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct Finite(f32);
impl Finite {
    /// Returns the checked value.
    pub fn get(self) -> f32 {
        self.0
    }
}
impl TryFrom<f32> for Finite {
    type Error = ErrorCode;
    fn try_from(value: f32) -> Result<Self, Self::Error> {
        if !value.is_finite() {
            return Err(ErrorCode::NotFinite);
        }
        if value.abs() > MAX_COORD {
            return Err(ErrorCode::OutOfRange);
        }
        Ok(Self(value))
    }
}
impl From<Finite> for f32 {
    fn from(value: Finite) -> Self {
        value.0
    }
}

/// A finite data number, not a canvas coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Number(f64);
impl Number {
    /// Returns the checked value.
    pub fn get(self) -> f64 {
        self.0
    }
}
impl TryFrom<f64> for Number {
    type Error = ErrorCode;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() {
            return Err(ErrorCode::NotFinite);
        }
        Ok(Self(value))
    }
}
impl From<Number> for f64 {
    fn from(value: Number) -> Self {
        value.0
    }
}

/// A sequence whose decoder rejects excess elements before storing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Bounded<T, const N: usize>(Vec<T>);
impl<T, const N: usize> Bounded<T, N> {
    /// Checks a programmatically built sequence.
    /// # Errors
    /// Returns `TooLong` when it exceeds the bound.
    pub fn new(values: Vec<T>) -> Result<Self, ErrorCode> {
        if values.len() > N {
            return Err(ErrorCode::TooLong);
        }
        Ok(Self(values))
    }
}
impl<T, const N: usize> Default for Bounded<T, N> {
    fn default() -> Self {
        Self(Vec::new())
    }
}
impl<T, const N: usize> Deref for Bounded<T, N> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for Bounded<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BoundedVisitor<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for BoundedVisitor<T, N> {
            type Value = Bounded<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded sequence")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element_seed(Element::<T> {
                    full: values.len() == N,
                    marker: std::marker::PhantomData,
                })? {
                    values.push(value);
                }
                Bounded::new(values).map_err(de::Error::custom)
            }
        }
        d.deserialize_seq(BoundedVisitor::<T, N>(std::marker::PhantomData))
    }
}

struct Element<T> {
    full: bool,
    marker: std::marker::PhantomData<T>,
}
impl<'de, T: Deserialize<'de>> de::DeserializeSeed<'de> for Element<T> {
    type Value = T;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<T, D::Error> {
        if self.full {
            return Err(de::Error::custom(ErrorCode::TooLong));
        }
        T::deserialize(d)
    }
}
