//! Durable issue actions (docs/development/issues.md): the stages a draft moves through and what
//! its card shows. The store commits every change; nothing here sends.

use std::fmt;
use std::num::NonZeroU64;

use kumochi_protocol::message::{
    ActionState, ApprovalRev, DraftId, FailReason, IssueBody, IssueRefused, IssueTitle, IssueUrl,
    LookupResult, RepoName, StateVersion, ThreadId,
};
use kumochi_scene::text::Bounded;
use serde::{Deserialize, Serialize};

use crate::connections::github::api::GitHubUser;

/// The most already-matching issue numbers a mark keeps (guess).
pub const MAX_PRESEND: usize = 200;

/// An issue action as its last commit left it. Only the store's transactions change it.
#[derive(Clone, PartialEq, Eq)]
pub struct Action {
    /// The durable id.
    pub draft: DraftId,
    /// The thread that owns it.
    pub thread: ThreadId,
    /// Raised by every commit.
    pub version: StateVersion,
    /// Where it is.
    pub stage: Stage,
    /// The bound repository, if one is chosen.
    pub repo: Option<RepoName>,
    /// The bound account, if any.
    pub account: Option<GitHubUser>,
    /// The exact title.
    pub title: IssueTitle,
    /// The exact normalized body.
    pub body: IssueBody,
    /// The revision a decision needs, offered only in Draft, Failed and Unknown.
    pub approval: Option<ApprovalRev>,
    /// The last revision a decision used up.
    pub consumed: Option<ApprovalRev>,
    /// Attempts that may have created an issue: 0 in Draft and Failed, at least 1 in Unknown.
    pub uncertain: u32,
}

/// Where an action is, stored as JSON tagged by `kind`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stage {
    /// Editable, waiting for consent.
    Draft,
    /// Admitted; the pre-send lookup runs and nothing has been sent.
    Preflight {
        /// The earliest mark of an earlier attempt that may have created an issue.
        unresolved: Option<Mark>,
    },
    /// The create request may have been sent.
    Posting {
        /// This attempt's mark.
        mark: Mark,
        /// The earliest mark of an earlier attempt that may have created an issue.
        unresolved: Option<Mark>,
    },
    /// The issue exists.
    Filed {
        /// Its number.
        number: NonZeroU64,
        /// The user confirmed an unplaced candidate.
        confirmed: bool,
    },
    /// No issue was created; a retry needs fresh consent.
    Failed {
        /// Why.
        reason: FailReason,
    },
    /// An attempt may have created an issue, so only File anyway sends again.
    Unknown {
        /// The earliest unresolved attempt's mark, whose window covers every later attempt.
        mark: Mark,
        /// The latest lookup.
        lookup: LookupResult,
    },
    /// Dismissed.
    Discarded,
}

/// What an attempt's preflight saw: when it began and the matching issues already there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Unix seconds when the preflight began.
    pub at_s: NonZeroU64,
    /// Matching issue numbers that existed before the send, which the lookup skips.
    pub presend: Bounded<NonZeroU64, MAX_PRESEND>,
}

/// What an action op came to.
pub enum Answer {
    /// Committed; the action as it now stands.
    Changed(Action),
    /// Nothing to change, so nothing was written.
    Unchanged(Action),
    /// Refused; the stage, content and binding stay as they were.
    Refused(IssueRefused, Action),
    /// No such action in a live thread.
    Gone,
}

impl Action {
    /// What the card shows. Preflight and Posting both show as Sending.
    ///
    /// # Panics
    /// If a filed action has no repository, which the schema forbids.
    pub fn state(&self) -> ActionState {
        match &self.stage {
            Stage::Draft => ActionState::Draft,
            Stage::Preflight { .. } | Stage::Posting { .. } => ActionState::Sending,
            Stage::Filed { number, confirmed } => ActionState::Filed {
                number: number.get(),
                url: self
                    .repo
                    .as_ref()
                    .and_then(|repo| IssueUrl::for_issue(repo, number.get()).ok())
                    .expect("the schema gives a filed action its repository"),
                confirmed_by_user: *confirmed,
            },
            Stage::Failed { reason } => ActionState::Failed(*reason),
            Stage::Unknown { lookup, .. } => ActionState::Unknown {
                lookup: lookup.clone(),
                maybe_duplicate: self.uncertain >= 2,
            },
            Stage::Discarded => ActionState::Discarded {
                maybe_filed: self.uncertain >= 1,
            },
        }
    }

    /// Replaces a Draft's or Failed action's content; a change makes it a Draft again.
    pub(crate) fn edit(
        &mut self,
        title: &IssueTitle,
        body: &IssueBody,
        repo: Option<&RepoName>,
    ) -> Result<(), IssueRefused> {
        if !matches!(self.stage, Stage::Draft | Stage::Failed { .. }) {
            return Err(IssueRefused::NotAllowedNow);
        }
        if (&self.title, &self.body, self.repo.as_ref()) != (title, body, repo) {
            (self.title, self.body, self.repo) = (title.clone(), body.clone(), repo.cloned());
            self.stage = Stage::Draft;
        }
        Ok(())
    }

    /// Dismisses an action that isn't sending or settled.
    pub(crate) fn discard(&mut self) -> Result<(), IssueRefused> {
        if !matches!(
            self.stage,
            Stage::Draft | Stage::Failed { .. } | Stage::Unknown { .. }
        ) {
            return Err(IssueRefused::NotAllowedNow);
        }
        self.stage = Stage::Discarded;
        Ok(())
    }
}

impl Stage {
    /// Where an interrupted send lands, with the new count of attempts that may have created an
    /// issue. Preflight sent nothing, so a first attempt fails; Posting may have sent. Either way
    /// the earliest unresolved mark stays. `None` when the count is exhausted.
    pub(crate) fn interrupted(self, uncertain: u32) -> Option<(Self, u32)> {
        Some(match self {
            Self::Preflight { unresolved: None } => (
                Self::Failed {
                    reason: FailReason::Interrupted,
                },
                uncertain,
            ),
            Self::Preflight {
                unresolved: Some(mark),
            } => (
                Self::Unknown {
                    mark,
                    lookup: LookupResult::Pending,
                },
                uncertain,
            ),
            Self::Posting { mark, unresolved } => (
                Self::Unknown {
                    mark: unresolved.unwrap_or(mark),
                    lookup: LookupResult::Pending,
                },
                uncertain.checked_add(1)?,
            ),
            settled => (settled, uncertain),
        })
    }
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Action")
            .field("draft", &self.draft)
            .field("thread", &self.thread)
            .field("version", &self.version)
            .field("stage", &self.stage)
            .field("repo", &self.repo)
            .field("account", &self.account)
            .field("title", &self.title)
            .field("body", &self.body)
            .field("approval", &self.approval)
            .field("consumed", &self.consumed)
            .field("uncertain", &self.uncertain)
            .finish()
    }
}

impl fmt::Debug for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Draft => f.write_str("Draft"),
            Self::Preflight { unresolved } => f
                .debug_struct("Preflight")
                .field("unresolved", &unresolved.is_some())
                .finish(),
            Self::Posting { unresolved, .. } => f
                .debug_struct("Posting")
                .field("unresolved", &unresolved.is_some())
                .finish_non_exhaustive(),
            Self::Filed { number, confirmed } => f
                .debug_struct("Filed")
                .field("number", number)
                .field("confirmed", confirmed)
                .finish(),
            Self::Failed { reason } => f.debug_tuple("Failed").field(reason).finish(),
            // An unplaced candidate's URL names the user's repository.
            Self::Unknown {
                lookup: LookupResult::Unplaced { number, .. },
                ..
            } => write!(f, "Unknown(Unplaced({number}))"),
            Self::Unknown { lookup, .. } => f.debug_tuple("Unknown").field(lookup).finish(),
            Self::Discarded => f.write_str("Discarded"),
        }
    }
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Changed(action) => f.debug_tuple("Changed").field(action).finish(),
            Self::Unchanged(action) => f.debug_tuple("Unchanged").field(action).finish(),
            Self::Refused(why, action) => {
                f.debug_tuple("Refused").field(why).field(action).finish()
            }
            Self::Gone => f.write_str("Gone"),
        }
    }
}
