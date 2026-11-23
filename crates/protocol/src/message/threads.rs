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
    /// The model's known supported efforts; unknown support permits no `ChatGPT` send.
    pub supported_efforts: Option<EffortSet>,
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

/// How hard the model is asked to think. The vocabulary doesn't say what a model accepts.
/// A stored effort this build doesn't know keeps its model, shows Medium and wants Low at
/// admission, so adding one needs no store migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

    fn bit(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 4,
            Self::XHigh => 8,
            Self::Max => 16,
        }
    }
}

/// A nonempty set of supported efforts, encoded as five checked bits in ascending order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct EffortSet(u8);

/// An effort set was empty or contained a bit outside the known vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an effort set must contain only known efforts and cannot be empty")]
pub struct EffortSetError;

impl EffortSet {
    /// Collects known efforts, collapsing duplicates; an empty iterator gives `None`.
    pub fn new(efforts: impl IntoIterator<Item = Effort>) -> Option<Self> {
        Self::try_from(
            efforts
                .into_iter()
                .fold(0, |bits, effort| bits | effort.bit()),
        )
        .ok()
    }

    /// Whether this set supports `effort`.
    #[must_use]
    pub fn contains(self, effort: Effort) -> bool {
        self.0 & effort.bit() != 0
    }

    /// The least effort this nonempty set supports.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "the private field and checked constructors exclude empty sets"
    )]
    pub fn lowest(self) -> Effort {
        self.iter()
            .next()
            .expect("EffortSet constructors reject empty sets")
    }

    /// The supported efforts in ascending order.
    pub fn iter(self) -> impl Iterator<Item = Effort> {
        Effort::ALL
            .into_iter()
            .filter(move |effort| self.contains(*effort))
    }
}

impl TryFrom<u8> for EffortSet {
    type Error = EffortSetError;

    fn try_from(bits: u8) -> Result<Self, Self::Error> {
        if bits == 0 || bits & !0b1_1111 != 0 {
            return Err(EffortSetError);
        }
        Ok(Self(bits))
    }
}

impl From<EffortSet> for u8 {
    fn from(set: EffortSet) -> Self {
        set.0
    }
}

/// The highest supported effort at or below `wanted`, or the lowest when none is below it.
#[must_use]
pub fn effective_effort(supported: EffortSet, wanted: Effort) -> Effort {
    Effort::ALL
        .into_iter()
        .rev()
        .find(|effort| *effort <= wanted && supported.contains(*effort))
        .unwrap_or_else(|| supported.lowest())
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

/// A change to one thread, shell to brain; model writes receive a committed result.
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
    fn effort_sets_reject_empty_and_unknown_bits_on_construction_and_the_wire() {
        assert_eq!(EffortSet::new([]), None);
        assert!(postcard::from_bytes::<EffortSet>(&[]).is_err());
        for bits in [0, 32, 33, 255] {
            assert_eq!(EffortSet::try_from(bits), Err(EffortSetError));
            assert!(postcard::from_bytes::<EffortSet>(&[bits]).is_err());
        }
        for (mask, effort) in [
            (1, Effort::Low),
            (2, Effort::Medium),
            (4, Effort::High),
            (8, Effort::XHigh),
            (16, Effort::Max),
        ] {
            let set = EffortSet::new([effort]).expect("one effort");
            assert_eq!(postcard::to_allocvec(&set), Ok(vec![mask]));
            assert_eq!(postcard::from_bytes::<EffortSet>(&[mask]), Ok(set));
            assert_eq!(set.iter().collect::<Vec<_>>(), [effort]);
        }
        let all = EffortSet::try_from(31).expect("all five bits");
        assert_eq!(
            all.iter().collect::<Vec<_>>(),
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::XHigh,
                Effort::Max
            ]
        );
        assert_eq!(postcard::to_allocvec(&all), Ok(vec![31]));
        assert_eq!(postcard::from_bytes::<EffortSet>(&[31]), Ok(all));
        let deduped =
            EffortSet::new([Effort::High, Effort::Low, Effort::High]).expect("two efforts");
        assert_eq!(postcard::to_allocvec(&deduped), Ok(vec![5]));
        assert_eq!(
            deduped.iter().collect::<Vec<_>>(),
            [Effort::Low, Effort::High]
        );
    }

    #[test]
    fn resolution_uses_the_highest_supported_effort_at_or_below_the_wanted_effort() {
        use Effort::{High, Low, Max, Medium, XHigh};
        for (bits, wanted, expected) in [
            (21, Medium, Low),
            (20, Medium, High),
            (21, XHigh, High),
            (20, Max, Max),
            (16, Low, Max),
        ] {
            let set = EffortSet::try_from(bits).expect("known nonempty mask");
            assert_eq!(
                effective_effort(set, wanted),
                expected,
                "{bits}: {wanted:?}"
            );
        }
    }

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
        let mut decoded: Vec<Effort> = (0..=u8::MAX)
            .filter_map(|tag| postcard::from_bytes(&[tag]).ok())
            .collect();
        decoded.sort_unstable();
        assert_eq!(decoded, Effort::ALL);
    }
}
