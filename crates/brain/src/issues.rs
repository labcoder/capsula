//! Durable issue actions (docs/development/issues.md): the stages a draft moves through and what
//! its card shows. The store commits every change; nothing here sends.

use std::fmt;
use std::num::NonZeroU64;

use kumochi_protocol::message::{
    ActionState, ApprovalRev, ContentHash, Decision, DraftId, FailReason, IssueBody, IssueRefused,
    IssueTitle, IssueUrl, LookupResult, RepoName, StateVersion, ThreadId,
};
use kumochi_scene::text::Bounded;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::connections::github::{api::GitHubUser, issues::CreateOutcome};

mod io;
pub use io::{Snapshot, Write};

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
        /// Retained duplicate-risk evidence; old records default to no retained warning.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        maybe_duplicate: bool,
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
#[derive(Clone, PartialEq, Eq)]
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

/// What admission checks a decision against, from the GitHub connection as it stands.
pub struct Live<'a> {
    /// The runtime reserved the one operation slot before this admission.
    pub capacity: bool,
    /// The verified connected user, from the device machine's `user()`.
    pub account: Option<&'a GitHubUser>,
    /// The repositories the connection lists.
    pub repos: &'a [RepoName],
}

/// A lookup's verdict on an action's uncertain attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// One match, placed after the mark.
    Placed(NonZeroU64),
    /// One match that can't be placed after the mark.
    Unplaced(NonZeroU64),
    /// No match.
    None,
    /// More than one match.
    Several,
    /// The lookup failed or was incomplete.
    Failed,
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
            Stage::Filed {
                number,
                confirmed,
                maybe_duplicate,
            } => ActionState::Filed {
                number: number.get(),
                url: self
                    .repo
                    .as_ref()
                    .and_then(|repo| IssueUrl::for_issue(repo, number.get()).ok())
                    .expect("the schema gives a filed action its repository"),
                confirmed_by_user: *confirmed,
                maybe_duplicate: *maybe_duplicate || self.uncertain >= 2,
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

    /// The consent binding: SHA-256 of the JSON of the repository, the account id, the title and
    /// the body, in that order. The login isn't bound; the id decides.
    ///
    /// # Panics
    /// Never: the fields are strings and integers, which always encode.
    pub fn hash(&self) -> ContentHash {
        #[derive(Serialize)]
        struct Bound<'a> {
            repo: Option<&'a RepoName>,
            account: Option<u64>,
            title: &'a IssueTitle,
            body: &'a IssueBody,
        }
        let json = serde_json::to_vec(&Bound {
            repo: self.repo.as_ref(),
            account: self.account.as_ref().map(|user| user.id),
            title: &self.title,
            body: &self.body,
        })
        .expect("strings and integers encode");
        ContentHash(Sha256::digest(json).into())
    }

    /// Applies a decision presented with `approval`. The last revision used up answers as done,
    /// and any other but the current one is stale. The current one is used up even when a check
    /// refuses it, so a stale click can't file once the cause is fixed.
    pub(crate) fn consent(
        &mut self,
        hash: &ContentHash,
        approval: ApprovalRev,
        decision: Decision,
        live: &Live<'_>,
    ) -> Result<(), IssueRefused> {
        if self.consumed == Some(approval) {
            return Ok(());
        }
        if self.approval != Some(approval) {
            return Err(IssueRefused::StaleApproval);
        }
        self.consumed = Some(approval);
        if !live.capacity {
            return Err(IssueRefused::NotAllowedNow);
        }
        let next = match (decision, &self.stage) {
            (Decision::File, Stage::Draft | Stage::Failed { .. }) => {
                Stage::Preflight { unresolved: None }
            }
            (Decision::FileAnyway, Stage::Unknown { mark, .. }) => Stage::Preflight {
                unresolved: Some(mark.clone()),
            },
            (
                Decision::ThisIsTheIssue,
                Stage::Unknown {
                    lookup: LookupResult::Unplaced { number, .. },
                    ..
                },
            ) => Stage::Filed {
                number: NonZeroU64::new(*number).ok_or(IssueRefused::NotAllowedNow)?,
                confirmed: true,
                maybe_duplicate: self.uncertain >= 2,
            },
            _ => return Err(IssueRefused::NotAllowedNow),
        };
        if *hash != self.hash() {
            return Err(IssueRefused::ContentChanged);
        }
        let (Some(bound), Some(connected)) =
            (&self.account, live.account.filter(|user| user.id != 0))
        else {
            return Err(IssueRefused::NotConnected);
        };
        if bound.id != connected.id {
            return Err(IssueRefused::OtherAccount);
        }
        if !self
            .repo
            .as_ref()
            .is_some_and(|repo| live.repos.contains(repo))
        {
            return Err(IssueRefused::RepoNotListed);
        }
        self.stage = next;
        Ok(())
    }

    /// Binds the connected account, and the default repository where none is chosen, to a
    /// Draft or Failed action; a change makes it a Draft again. It never files.
    pub(crate) fn bind(
        &mut self,
        account: Option<&GitHubUser>,
        repo: Option<&RepoName>,
    ) -> Result<(), IssueRefused> {
        if !matches!(self.stage, Stage::Draft | Stage::Failed { .. }) {
            return Err(IssueRefused::NotAllowedNow);
        }
        let account = account.cloned().or_else(|| self.account.clone());
        let repo = self.repo.clone().or_else(|| repo.cloned());
        if (&account, &repo) != (&self.account, &self.repo) {
            (self.account, self.repo, self.stage) = (account, repo, Stage::Draft);
        }
        Ok(())
    }

    /// Ends the Preflight dispatched at `at`: a mark lets the create go out, and a refusal means
    /// nothing was sent.
    pub(crate) fn preflighted(
        &mut self,
        at: StateVersion,
        result: Result<Mark, FailReason>,
    ) -> Result<(), IssueRefused> {
        let (Stage::Preflight { unresolved }, true) = (&self.stage, self.version == at) else {
            return Err(IssueRefused::NotAllowedNow);
        };
        self.stage = match result {
            Ok(mark) => Stage::Posting {
                mark,
                unresolved: unresolved.clone(),
            },
            Err(reason) => Stage::unsent(unresolved.clone(), reason),
        };
        Ok(())
    }

    /// Records the create dispatched at `at` as P3b reported it. A zero number proves nothing,
    /// so it counts as uncertain. `None` when the count is exhausted.
    pub(crate) fn posted(
        &mut self,
        at: StateVersion,
        outcome: &CreateOutcome,
    ) -> Option<Result<(), IssueRefused>> {
        let (Stage::Posting { unresolved, .. }, true) = (&self.stage, self.version == at) else {
            return Some(Err(IssueRefused::NotAllowedNow));
        };
        let number = match outcome {
            CreateOutcome::Created { number, .. } => NonZeroU64::new(*number),
            _ => None,
        };
        match (number, outcome) {
            (Some(number), _) => {
                self.stage = Stage::Filed {
                    number,
                    confirmed: false,
                    maybe_duplicate: self.uncertain >= 1,
                };
            }
            (None, CreateOutcome::Definite { reason, .. }) => {
                self.stage = Stage::unsent(unresolved.clone(), *reason);
            }
            (None, _) => {
                (self.stage, self.uncertain) = self.stage.clone().interrupted(self.uncertain)?;
            }
        }
        Some(Ok(()))
    }

    /// Ends an attempt whose transport handoff definitely never happened.
    pub(crate) fn not_sent(
        &mut self,
        at: StateVersion,
        reason: FailReason,
    ) -> Result<(), IssueRefused> {
        if self.version != at {
            return Err(IssueRefused::NotAllowedNow);
        }
        let (Stage::Preflight { unresolved } | Stage::Posting { unresolved, .. }) = &self.stage
        else {
            return Err(IssueRefused::NotAllowedNow);
        };
        self.stage = Stage::unsent(unresolved.clone(), reason);
        Ok(())
    }

    /// Ends the send dispatched at `at` that the runtime gave up on, as recovery would.
    /// `None` when the count is exhausted.
    pub(crate) fn interrupted(&mut self, at: StateVersion) -> Option<Result<(), IssueRefused>> {
        if self.version != at
            || !matches!(self.stage, Stage::Preflight { .. } | Stage::Posting { .. })
        {
            return Some(Err(IssueRefused::NotAllowedNow));
        }
        (self.stage, self.uncertain) = self.stage.clone().interrupted(self.uncertain)?;
        Some(Ok(()))
    }

    /// Applies a lookup run for the uncertain attempts counted as `episode`. A refused File
    /// anyway moves the version but not the count, so the lookup in flight still lands.
    pub(crate) fn looked_up(&mut self, episode: u32, lookup: Lookup) -> Result<(), IssueRefused> {
        let (Stage::Unknown { mark, .. }, true) = (&self.stage, self.uncertain == episode) else {
            return Err(IssueRefused::NotAllowedNow);
        };
        let mark = mark.clone();
        let lookup = match lookup {
            Lookup::Placed(number) => {
                self.stage = Stage::Filed {
                    number,
                    confirmed: false,
                    maybe_duplicate: self.uncertain >= 2,
                };
                return Ok(());
            }
            Lookup::Unplaced(number) => LookupResult::Unplaced {
                number: number.get(),
                url: self
                    .repo
                    .as_ref()
                    .and_then(|repo| IssueUrl::for_issue(repo, number.get()).ok())
                    .ok_or(IssueRefused::NotAllowedNow)?,
            },
            Lookup::None => LookupResult::None,
            Lookup::Several => LookupResult::Several,
            Lookup::Failed => LookupResult::Failed,
        };
        self.stage = Stage::Unknown { mark, lookup };
        Ok(())
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
            Self::Preflight { unresolved } => {
                (Self::unsent(unresolved, FailReason::Interrupted), uncertain)
            }
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

    /// Where an attempt that sent nothing lands: a first attempt fails, and an earlier attempt
    /// that may have created an issue keeps the action Unknown.
    fn unsent(unresolved: Option<Mark>, reason: FailReason) -> Self {
        match unresolved {
            None => Self::Failed { reason },
            Some(mark) => Self::Unknown {
                mark,
                lookup: LookupResult::Pending,
            },
        }
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
            Self::Filed {
                number,
                confirmed,
                maybe_duplicate,
            } => f
                .debug_struct("Filed")
                .field("number", number)
                .field("confirmed", confirmed)
                .field("maybe_duplicate", maybe_duplicate)
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

impl fmt::Debug for Live<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Live")
            .field("capacity", &self.capacity)
            .field("connected", &self.account.is_some())
            .field("repos", &self.repos.len())
            .finish()
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
