//! Issue consent and presentation values; the brain owns their durable transitions.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{GitHubLogin, IssueValueError, RepoName, ThreadId};

/// The title's local limit, in Unicode scalar values (guess).
pub const MAX_ISSUE_TITLE_CHARS: usize = 256;
/// The normalized body's local UTF-8 byte limit (guess).
pub const MAX_ISSUE_BODY_BYTES: usize = 32 * 1024;
/// The most cards requested in one frame (guess).
pub const MAX_ISSUE_CARDS: usize = 16;

checked_text!(IssueTitle, title, "An untrimmed, one-line issue title.");
checked_text!(IssueUrl, issue_url, "A canonical GitHub issue URL.");

fn title(text: &str) -> Result<(), IssueValueError> {
    if text.trim().is_empty()
        || text.chars().count() > MAX_ISSUE_TITLE_CHARS
        || text
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(IssueValueError);
    }
    Ok(())
}

/// An LF-normalized issue body. Decoding refuses noncanonical CRLF wire text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IssueBody(String);

impl IssueBody {
    /// Normalizes CRLF to LF, then checks the byte and control-character bounds.
    ///
    /// # Errors
    /// Refuses excess bytes or controls other than LF and tab.
    pub fn new(text: &str) -> Result<Self, IssueValueError> {
        Self::try_from(text.replace("\r\n", "\n"))
    }

    /// The canonical body, never diagnostic content.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IssueBody {
    type Error = IssueValueError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.len() > MAX_ISSUE_BODY_BYTES
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err(IssueValueError);
        }
        Ok(Self(text))
    }
}

impl From<IssueBody> for String {
    fn from(value: IssueBody) -> Self {
        value.0
    }
}

impl fmt::Debug for IssueBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IssueBody(bytes={})", self.0.len())
    }
}

impl IssueUrl {
    /// Builds an issue URL, never accepting an arbitrary host or path.
    ///
    /// # Errors
    /// Refuses a zero issue number.
    pub fn for_issue(repo: &RepoName, number: u64) -> Result<Self, IssueValueError> {
        if number == 0 {
            return Err(IssueValueError);
        }
        Ok(Self(format!(
            "https://github.com/{}/issues/{number}",
            repo.as_str()
        )))
    }
}

fn issue_url(text: &str) -> Result<(), IssueValueError> {
    let path = text
        .strip_prefix("https://github.com/")
        .ok_or(IssueValueError)?;
    let (repo, number) = path.rsplit_once("/issues/").ok_or(IssueValueError)?;
    let repo = RepoName::new(repo)?;
    let number = number.parse::<u64>().map_err(|_| IssueValueError)?;
    if IssueUrl::for_issue(&repo, number)?.as_str() != text {
        return Err(IssueValueError);
    }
    Ok(())
}

/// One durable issue draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DraftId(pub u64);
/// A brain-issued approval revision, never reused across restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRev(pub u64);
/// An action's monotonic presentation version, distinct from its approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StateVersion(pub u64);
/// The brain's hash binding the exact approved content and account.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentHash(..)")
    }
}

/// A decision available only through the user's consent surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// File a draft or retry a definite failure.
    File,
    /// File despite an uncertain earlier outcome.
    FileAnyway,
    /// Confirm the lookup's unplaced candidate.
    ThisIsTheIssue,
}

/// The user's action on a draft; no tool may manufacture consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueOp {
    /// Confirm the exact content under a brain-issued revision.
    Consent {
        /// The draft.
        draft: DraftId,
        /// The content binding the user saw.
        hash: ContentHash,
        /// The revision offered on the card.
        approval: ApprovalRev,
        /// The explicit decision.
        decision: Decision,
    },
    /// Dismiss the draft without creating an issue.
    Discard {
        /// The draft.
        draft: DraftId,
    },
    /// Look up an uncertain outcome, never resend it.
    CheckAgain {
        /// The draft.
        draft: DraftId,
    },
}

/// A definite failure, without provider diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailReason {
    /// GitHub isn't connected.
    NotConnected,
    /// The connected account lacks permission.
    PermissionDenied,
    /// The repository couldn't be found.
    NotFound,
    /// GitHub refused the issue's content.
    InvalidIssue,
    /// GitHub requires waiting before another attempt.
    RateLimited,
    /// A connection failed before sending.
    Network,
    /// The pre-send lookup couldn't establish a safe mark.
    PreflightFailed,
    /// Recovery or a generation change interrupted admission before sending.
    Interrupted,
    /// Another definite GitHub refusal.
    Rejected,
}

/// An inconclusive lookup; an unplaced candidate is never automatically Filed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookupResult {
    /// The lookup hasn't completed.
    Pending,
    /// No matching issue was visible; absence is not proof of no send.
    None,
    /// More than one issue matched.
    Several,
    /// The lookup failed or was incomplete.
    Failed,
    /// One candidate exists, but its timestamp cannot place it after the send mark.
    Unplaced {
        /// The candidate's issue number.
        number: u64,
        /// The candidate's canonical URL.
        url: IssueUrl,
    },
}

/// The state shown on the card; Sending's durable phase stays brain-local.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionState {
    /// Editable, awaiting explicit consent.
    Draft,
    /// Preflight or posting is in progress.
    Sending,
    /// A confirmed remote issue.
    Filed {
        /// The issue number.
        number: u64,
        /// The canonical issue URL.
        url: IssueUrl,
        /// The user explicitly confirmed an unplaced candidate.
        confirmed_by_user: bool,
    },
    /// A definite failure; a retry requires fresh consent.
    Failed(FailReason),
    /// An uncertain send, never automatically retried.
    Unknown {
        /// The current lookup outcome, including any candidate.
        lookup: LookupResult,
        /// This attempt followed a File anyway decision.
        maybe_duplicate: bool,
    },
    /// Dismissed, preserving any uncertainty about a remote issue.
    Discarded {
        /// The dismissed action may already exist on GitHub.
        maybe_filed: bool,
    },
}

/// A refusal leaves the stored action and its presentation state unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueRefused {
    /// The approval isn't current.
    StaleApproval,
    /// The content hash no longer matches.
    ContentChanged,
    /// There is no usable connection.
    NotConnected,
    /// The draft belongs to a different account.
    OtherAccount,
    /// The repository isn't in the installed list.
    RepoNotListed,
    /// This state doesn't allow that decision.
    NotAllowedNow,
    /// The thread was deleted or changed generation.
    ThreadGone,
}

/// The exact content and current consent binding displayed on one card.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCard {
    /// The durable action id.
    pub draft: DraftId,
    /// The owning live thread.
    pub thread: ThreadId,
    /// The bound repository, if selected.
    pub repo: Option<RepoName>,
    /// The bound account, if connected.
    pub account: Option<GitHubLogin>,
    /// The exact approved title.
    pub title: IssueTitle,
    /// The exact normalized body.
    pub body: IssueBody,
    /// The brain's content binding.
    pub hash: ContentHash,
    /// The current approval, absent when no decision is offered.
    pub approval: Option<ApprovalRev>,
    /// The action's presentation version.
    pub version: StateVersion,
    /// The current outcome.
    pub state: ActionState,
}

impl fmt::Debug for IssueCard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueCard")
            .field("draft", &self.draft)
            .field("thread", &self.thread)
            .field("title_chars", &self.title.as_str().chars().count())
            .field("body_bytes", &self.body.as_str().len())
            .field("version", &self.version)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
