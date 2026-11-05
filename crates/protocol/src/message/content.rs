//! Text the user typed or a model wrote, as the messages and the brain's effects carry it.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// What the user typed or a model wrote. On the wire it's a plain string; its `Debug` shows
/// only the length, so a message, effect or store write that carries it never prints it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Content(String);

impl Content {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The text, owned.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Adds `text` at the end.
    pub fn push_str(&mut self, text: &str) {
        self.0.push_str(text);
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl Deref for Content {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Content {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Content {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Content(len={})", self.0.len())
    }
}
