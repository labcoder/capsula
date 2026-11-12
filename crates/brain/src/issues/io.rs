//! Typed issue operations for the existing store thread; filing policy stays in the runtime.

use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::time::Instant;

use kumochi_protocol::message::{
    ApprovalRev, ContentHash, Decision, DraftId, FailReason, RepoName, StateVersion,
};

use super::{Live, Lookup, Mark};
use crate::connections::github::{api::GitHubUser, issues::CreateOutcome};

/// The original operation deadline and shared cancellation for an admitted SQL phase.
#[derive(Clone, Debug)]
pub struct WriteScope {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl PartialEq for WriteScope {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

impl Eq for WriteScope {}

impl WriteScope {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }
}

/// Owned account metadata captured when the runtime reserves or refuses an operation slot.
#[derive(Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The operation slot was reserved before this write was queued.
    pub capacity: bool,
    /// The verified, unexpired connected identity, without credentials.
    pub account: Option<GitHubUser>,
    /// The complete repository authority, or an empty list when unavailable.
    pub repos: Vec<RepoName>,
}

impl Snapshot {
    pub(crate) fn live(&self) -> Live<'_> {
        Live {
            capacity: self.capacity,
            account: self.account.as_ref(),
            repos: &self.repos,
        }
    }
}

/// One ordered read or transactional change, answered with the authoritative action.
#[derive(Clone, PartialEq, Eq)]
pub enum Write {
    /// Read the live action before a GET-only recovery operation.
    Read(DraftId),
    /// Supersede a review revision without changing the saved draft's content or stage.
    Rearm {
        /// The action.
        draft: DraftId,
        /// The immutable version the review is based on.
        at: StateVersion,
        /// The exact displayed content binding.
        hash: ContentHash,
    },
    /// Consume one consent revision, including a refused admission.
    Consent {
        /// The action.
        draft: DraftId,
        /// The immutable version registered with this review.
        at: StateVersion,
        /// The exact displayed content binding.
        hash: ContentHash,
        /// The displayed single-use revision.
        approval: ApprovalRev,
        /// The explicit user decision.
        decision: Decision,
        /// Current authority and the reserved capacity.
        live: Snapshot,
    },
    /// Revalidate canceled File consent and atomically record its known-unsent failure.
    CancelledConsent {
        /// The action.
        draft: DraftId,
        /// The immutable version registered with the original review.
        at: StateVersion,
        /// The original displayed content binding.
        hash: ContentHash,
        /// The original single-use revision.
        approval: ApprovalRev,
        /// Authority and capacity captured at the original admission.
        live: Snapshot,
    },
    /// Save preflight evidence before any create can be dispatched.
    Preflighted {
        /// The action.
        draft: DraftId,
        /// The acknowledged Preflight version.
        at: StateVersion,
        /// Complete evidence or a known-unsent refusal.
        result: Result<Mark, FailReason>,
    },
    /// Record the result of the single create request.
    Posted {
        /// The action.
        draft: DraftId,
        /// The acknowledged Posting version.
        at: StateVersion,
        /// The typed transport result, never provider text.
        outcome: CreateOutcome,
    },
    /// Stop after cancellation when transport handoff may have happened.
    Interrupted {
        /// The action.
        draft: DraftId,
        /// The acknowledged in-flight version.
        at: StateVersion,
    },
    /// Stop with positive runtime evidence that no create reached the transport.
    NotSent {
        /// The action.
        draft: DraftId,
        /// The acknowledged in-flight version.
        at: StateVersion,
        /// The fixed reason, without foreign diagnostics.
        reason: FailReason,
    },
    /// Save a GET-only lookup, never an inferred successful filing.
    LookedUp {
        /// The action.
        draft: DraftId,
        /// The uncertain episode read when this job began.
        episode: u32,
        /// Candidate-only evidence from the runtime.
        lookup: Lookup,
    },
    /// Hide an action without sending anything.
    Discard(DraftId),
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueSnapshot")
            .field("capacity", &self.capacity)
            .field("connected", &self.account.is_some())
            .field("repos", &self.repos.len())
            .finish()
    }
}

impl fmt::Debug for Write {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read(_) => "IssueRead",
            Self::Rearm { .. } => "IssueRearm",
            Self::Consent { .. } => "IssueConsent",
            Self::CancelledConsent { .. } => "IssueCancelledConsent",
            Self::Preflighted { .. } => "IssuePreflighted",
            Self::Posted { .. } => "IssuePosted",
            Self::Interrupted { .. } => "IssueInterrupted",
            Self::NotSent { .. } => "IssueNotSent",
            Self::LookedUp { .. } => "IssueLookedUp",
            Self::Discard(_) => "IssueDiscard",
        })
    }
}
