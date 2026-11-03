//! The user's settings as they travel between shell and brain. Names carry user content, so
//! their `Debug` prints only a length.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::ThreadId;

/// The longest name, in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 24;

/// Why a name or a thread title was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Nothing but whitespace.
    #[error("the text is empty")]
    Empty,
    /// More characters than its kind allows: [`MAX_NAME_CHARS`] for a name.
    #[error("the text is too long")]
    TooLong,
    /// A control character or a line break: names and titles go into requests and one-line rows.
    #[error("the text isn't one line")]
    NotOneLine,
}

/// The shared rule: trimmed, 1–`max` scalar values, one line, no control characters.
pub(super) fn checked(text: &str, max: usize) -> Result<String, NameError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(NameError::Empty);
    }
    if text
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(NameError::NotOneLine);
    }
    if text.chars().count() > max {
        return Err(NameError::TooLong);
    }
    Ok(text.to_owned())
}

/// A checked text type: built only through [`checked`] with `$max`, on decode too, and with a
/// `Debug` that prints only its length.
macro_rules! name {
    ($(#[$doc:meta])* $name:ident, $max:expr) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Checks `text` against the name rule, trimming it first.
            ///
            /// # Errors
            /// [`NameError`] says which part of the rule it breaks.
            pub fn new(text: &str) -> Result<Self, NameError> {
                checked(text, $max).map(Self)
            }

            /// The name's text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(text: String) -> Result<Self, NameError> {
                Self::new(&text)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> String {
                name.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(len={})", stringify!($name), self.0.chars().count())
            }
        }
    };
}

name! {
    /// What the assistant is called, in every request and on Kumo's row.
    AssistantName, MAX_NAME_CHARS
}

name! {
    /// What the assistant calls the user. Optional: unset is `None`, never an empty name.
    UserName, MAX_NAME_CHARS
}

/// A settings change, shell to brain. Each one is answered by exactly one
/// [`super::ControlToShell::Settings`] with `answer` set, in order, while its connection lasts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingsOp {
    /// Rename the assistant.
    SetAssistantName(AssistantName),
    /// Set or clear the user's name.
    SetUserName(Option<UserName>),
    /// Onboarding is done; it doesn't show again.
    FinishOnboarding,
}

/// The settings as the brain holds them: once per connection before `Scenes`, and after
/// every [`SettingsOp`], applied or refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    /// The assistant's name.
    pub assistant_name: AssistantName,
    /// The user's name, if set.
    pub user_name: Option<UserName>,
    /// Onboarding has finished.
    pub onboarded: bool,
    /// The main thread, Kumo's, which the shell labels with `assistant_name`.
    pub main_thread: ThreadId,
}

#[cfg(test)]
mod tests {
    use super::{AssistantName, MAX_NAME_CHARS, NameError, UserName};

    #[test]
    fn names_follow_the_one_rule() {
        let ok = |text: &str| AssistantName::new(text).map(|name| name.as_str().to_owned());
        assert_eq!(ok("K"), Ok("K".to_owned()));
        assert_eq!(ok("  Kumo \t"), Ok("Kumo".to_owned()));
        assert_eq!(ok(&"a".repeat(MAX_NAME_CHARS)), Ok("a".repeat(24)));
        assert_eq!(
            ok(&"🌥".repeat(24)),
            Ok("🌥".repeat(24)),
            "24 four-byte characters"
        );
        assert_eq!(ok(&"a".repeat(25)), Err(NameError::TooLong));
        assert_eq!(ok(""), Err(NameError::Empty));
        assert_eq!(ok("   "), Err(NameError::Empty));
        for bad in [
            "Ku\nmo",
            "Ku\u{7}mo",
            "Ku\u{2028}mo",
            "Ku\u{2029}mo",
            "Ku\rmo",
        ] {
            assert_eq!(ok(bad), Err(NameError::NotOneLine), "{bad:?}");
        }
        assert_eq!(UserName::new("").map(|_| ()), Err(NameError::Empty));
    }

    #[test]
    fn an_invalid_name_fails_to_decode() {
        let bytes = postcard::to_allocvec(&"a".repeat(25)).expect("encodes");
        assert!(postcard::from_bytes::<AssistantName>(&bytes).is_err());
        let bytes = postcard::to_allocvec(&"Kumo").expect("encodes");
        let name: AssistantName = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(name.as_str(), "Kumo");
    }

    #[test]
    fn debug_prints_only_the_length() {
        let name = AssistantName::new("Sentinelname").expect("valid");
        assert_eq!(format!("{name:?}"), "AssistantName(len=12)");
        let user = Some(UserName::new("Oscarsentinel").expect("valid"));
        assert!(!format!("{user:?}").contains("Oscarsentinel"));
    }
}
