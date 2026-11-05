//! What a thread is besides its events: its kind, its title and the model it runs on, and the
//! shell's ops that change them. Titles and model slugs print only their lengths in `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::settings::checked;
use super::{ConnectionKind, NameError, ThreadId};

/// The longest thread title, in Unicode scalar values.
pub const MAX_TITLE_CHARS: usize = 80;
/// The longest model slug, in bytes.
pub const MAX_SLUG_BYTES: usize = 64;
/// The most models one catalog lists [local policy].
pub const MAX_MODELS: usize = 200;
/// The longest model display name, in Unicode scalar values [local policy].
pub const MAX_MODEL_NAME_CHARS: usize = 80;
/// Every listed model is treated as able to call tools [local policy, the Captain's pick (b)]:
/// the model list doesn't say, so a provider's refusal shows at run time instead.
pub const LISTED_MODELS_CALL_TOOLS: bool = true;

/// What a thread is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadKind {
    /// The one main thread, Kumo's: never deleted, and first on the plain list's first page.
    Main,
    /// A task.
    Thread,
}

name! {
    /// A task's title as the user or a model set it.
    ThreadTitle, MAX_TITLE_CHARS
}

name! {
    /// A model's name as the catalog shows it.
    ModelName, MAX_MODEL_NAME_CHARS
}

/// One model the account's catalog lists, in the catalog's order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// What a run sends.
    pub slug: ModelSlug,
    /// What the picker shows.
    pub display_name: ModelName,
}

/// A model slug broke the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a model slug is 1–64 bytes of a–z, 0–9, '.', '_' and '-'")]
pub struct SlugError;

/// A model's id as the catalog names it: 1–[`MAX_SLUG_BYTES`] bytes of `[a-z0-9._-]`. Whether
/// the catalog still offers it is checked per run, not here.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelSlug(String);

impl fmt::Debug for ModelSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModelSlug(len={})", self.0.len())
    }
}

impl ModelSlug {
    /// Checks `text` against the slug rule.
    ///
    /// # Errors
    /// [`SlugError`] when it's empty, too long, or holds any other byte.
    pub fn new(text: &str) -> Result<Self, SlugError> {
        Self::try_from(text.to_owned())
    }

    /// The slug's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelSlug {
    type Error = SlugError;

    fn try_from(text: String) -> Result<Self, SlugError> {
        let allowed = |byte: u8| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-');
        if (1..=MAX_SLUG_BYTES).contains(&text.len()) && text.bytes().all(allowed) {
            Ok(Self(text))
        } else {
            Err(SlugError)
        }
    }
}

impl From<ModelSlug> for String {
    fn from(slug: ModelSlug) -> String {
        slug.0
    }
}

/// How hard the model thinks: the documented effort vocabulary, which isn't proof that a model
/// accepts a value, so a refusal shows at run time. A stored effort a build doesn't know reads as
/// no choice, so adding one needs no store migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    /// Less than usual.
    Low,
    /// The usual.
    Medium,
    /// More than usual.
    High,
    /// Much more than usual.
    XHigh,
    /// The most.
    Max,
}

impl Effort {
    /// Every effort, in ascending order.
    pub const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];
}

/// The model and effort a thread runs on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// The model.
    pub model: ModelSlug,
    /// Its effort.
    pub effort: Effort,
    /// The connection it runs on. A choice saved before Claude existed reads as `ChatGpt`; postcard
    /// still always encodes it.
    #[serde(default)]
    pub connection: ConnectionKind,
}

/// A change to one thread, shell to brain; nothing answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadOp {
    /// Retitle a task; the user's title wins over any other. A rename of the main thread is
    /// refused: its name is the assistant's, changed with `SettingsOp::SetAssistantName`.
    Rename {
        /// The task.
        thread: ThreadId,
        /// Its new title.
        title: ThreadTitle,
    },
    /// Store the model and effort a thread runs on.
    SetModel {
        /// The thread.
        thread: ThreadId,
        /// The model and effort.
        choice: ModelChoice,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_one_trimmed_line_of_1_to_80_characters() {
        let ok = |text: &str| ThreadTitle::new(text).map(|title| title.as_str().to_owned());
        assert_eq!(ok("  Trip plan \t"), Ok("Trip plan".to_owned()));
        assert_eq!(ok(&"界".repeat(MAX_TITLE_CHARS)), Ok("界".repeat(80)));
        assert_eq!(ok(&"a".repeat(81)), Err(NameError::TooLong));
        assert_eq!(ok(" \n "), Err(NameError::Empty));
        for bad in ["Trip\nplan", "Trip\u{7}plan", "Trip\u{2028}plan"] {
            assert_eq!(ok(bad), Err(NameError::NotOneLine), "{bad:?}");
        }
        let bytes = postcard::to_allocvec(&"a".repeat(81)).expect("encodes");
        assert_eq!(
            postcard::from_bytes::<ThreadTitle>(&bytes),
            Err(postcard::Error::SerdeDeCustom)
        );
    }

    #[test]
    fn slugs_are_1_to_64_bytes_of_lowercase_digits_dots_underscores_and_dashes() {
        let ok = |text: &str| ModelSlug::new(text).map(|slug| slug.as_str().to_owned());
        assert_eq!(ok("gpt-6.1_sol"), Ok("gpt-6.1_sol".to_owned()));
        assert_eq!(ok(&"a".repeat(MAX_SLUG_BYTES)), Ok("a".repeat(64)));
        for bad in ["", "GPT", "gpt 6", "gpt/6", "é", &"a".repeat(65)] {
            assert_eq!(ok(bad), Err(SlugError), "{bad:?}");
        }
        let bytes = postcard::to_allocvec(&"Not-A-Slug").expect("encodes");
        assert_eq!(
            postcard::from_bytes::<ModelSlug>(&bytes),
            Err(postcard::Error::SerdeDeCustom)
        );
    }

    #[test]
    fn every_effort_that_decodes_is_in_all() {
        let decoded: Vec<Effort> = (0..=u8::MAX)
            .filter_map(|tag| postcard::from_bytes(&[tag]).ok())
            .collect();
        assert!(decoded.iter().all(|effort| Effort::ALL.contains(effort)));
        assert_eq!(decoded.len(), Effort::ALL.len());
    }

    #[test]
    fn a_rename_prints_only_its_titles_length() {
        let rename = ThreadOp::Rename {
            thread: ThreadId(4),
            title: ThreadTitle::new("Zqxsecrettitle").expect("valid"),
        };
        assert_eq!(
            format!("{rename:?}"),
            "Rename { thread: ThreadId(4), title: ThreadTitle(len=14) }"
        );
    }
}
