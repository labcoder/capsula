//! Checked GitHub connection data. Credentials never travel to the shell.

use std::fmt;

use kumochi_scene::text::Bounded;
use serde::{Deserialize, Serialize};

use super::Cleanup;

/// The most repositories advertised in one connection view (guess).
pub const MAX_GITHUB_REPOS: usize = 200;

/// A content-free refusal of a GitHub or issue wire value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid github or issue value")]
pub struct IssueValueError;

macro_rules! checked_text {
    ($name:ident, $check:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates without changing the text.
            ///
            /// # Errors
            /// Refuses text outside this type's grammar or bounds.
            pub fn new(text: &str) -> Result<Self, IssueValueError> {
                $check(text)?;
                Ok(Self(text.to_owned()))
            }

            /// The checked text, never diagnostic content.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IssueValueError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                $check(&text)?;
                Ok(Self(text))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(bytes={})", stringify!($name), self.0.len())
            }
        }
    };
}

checked_text!(RepoName, repo, "A checked owner/repository pair.");
checked_text!(GitHubLogin, login, "A checked GitHub login.");
checked_text!(UserCode, code, "The user's GitHub device-flow code.");

fn login(text: &str) -> Result<(), IssueValueError> {
    if !(1..=39).contains(&text.len())
        || text.starts_with('-')
        || !text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(IssueValueError);
    }
    Ok(())
}

fn repo(text: &str) -> Result<(), IssueValueError> {
    let (owner, name) = text.split_once('/').ok_or(IssueValueError)?;
    login(owner)?;
    if !(1..=100).contains(&name.len())
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(IssueValueError);
    }
    Ok(())
}

fn code(text: &str) -> Result<(), IssueValueError> {
    if !(1..=16).contains(&text.len())
        || !text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(IssueValueError);
    }
    Ok(())
}

/// A user operation on Kumochi's own GitHub connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitHubOp {
    /// Begin a new device-flow attempt.
    Connect,
    /// Cancel the current attempt.
    CancelConnect,
    /// Fence local use and remove the saved sign-in.
    SignOut,
    /// Retry removal after a credential-store failure.
    RetryCleanup,
    /// Set the repository used for new drafts.
    SetDefaultRepo(RepoName),
    /// Refetch the repositories the connected account can use.
    RefreshRepos,
}

/// Connection presentation, without any bearer or device credential.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitHubView {
    /// No usable connection, even if credential cleanup failed.
    SignedOut {
        /// Whether local credential removal completed.
        cleanup: Cleanup,
    },
    /// The initial device-code request is pending.
    Requesting,
    /// The user must enter this code on GitHub's fixed device page.
    AwaitingUser {
        /// The displayed code, never diagnostic text.
        user_code: UserCode,
        /// Remaining seconds before the attempt expires.
        expires_in_s: u16,
    },
    /// An authenticated account and its installed repository intersection.
    Connected {
        /// The connected account.
        login: GitHubLogin,
        /// The bounded repository list.
        repos: Bounded<RepoName, MAX_GITHUB_REPOS>,
        /// More repositories exist beyond this view's limit.
        truncated: bool,
        /// The saved default, if selected.
        default: Option<RepoName>,
    },
    /// The account has no usable app installation.
    NotInstalled {
        /// The authenticated account.
        login: GitHubLogin,
    },
    /// The account must authorize again.
    NeedsSignIn {
        /// The account previously connected.
        login: GitHubLogin,
    },
}

impl fmt::Debug for GitHubView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignedOut { cleanup } => f
                .debug_struct("SignedOut")
                .field("cleanup", cleanup)
                .finish(),
            Self::Requesting => f.write_str("Requesting"),
            Self::AwaitingUser { expires_in_s, .. } => f
                .debug_struct("AwaitingUser")
                .field("expires_in_s", expires_in_s)
                .finish_non_exhaustive(),
            Self::Connected {
                repos, truncated, ..
            } => f
                .debug_struct("Connected")
                .field("repos", &repos.len())
                .field("truncated", truncated)
                .finish_non_exhaustive(),
            Self::NotInstalled { .. } => f.write_str("NotInstalled(..)"),
            Self::NeedsSignIn { .. } => f.write_str("NeedsSignIn(..)"),
        }
    }
}
