use kumochi_protocol::message::{
    ApprovalRev, ContentHash, DataToShell, DraftId, IssueCard, IssueSession, IssueUnavailableKind,
    MAX_ISSUE_REVIEWS, StateVersion,
};
use tokio::time::Instant;

use super::{Brain, Effect, Filing, StoreOp};
use crate::issues::{Answer, Stage, Write, WriteScope};
use crate::runtime::account::github::repositories::FILING_DEADLINE;

#[derive(Debug)]
pub(super) struct Offered {
    pub(super) session: IssueSession,
    pub(super) review: u64,
    pub(super) draft: DraftId,
    pub(super) version: StateVersion,
    pub(super) hash: ContentHash,
    pub(super) approval: ApprovalRev,
}

#[derive(Debug)]
pub(super) struct Rearm {
    pub(super) serial: u64,
    epoch: u64,
    session: IssueSession,
    request: u64,
    draft: DraftId,
    version: StateVersion,
    hash: ContentHash,
    pub(super) deadline: Instant,
    scope: WriteScope,
    pub(super) live: bool,
}

impl Filing {
    pub(super) fn cancel_reviews(&mut self) {
        self.offers.clear();
        for rearm in &mut self.rearms {
            rearm.live = false;
            rearm.scope.cancel();
        }
    }

    pub(super) fn cancel_review(&mut self, session: IssueSession, request: u64, draft: DraftId) {
        self.offers.retain(|offer| {
            offer.session != session || offer.review != request || offer.draft != draft
        });
        for rearm in &mut self.rearms {
            if rearm.session == session && rearm.request == request && rearm.draft == draft {
                rearm.live = false;
                rearm.scope.cancel();
            }
        }
    }

    fn supersede_review(&mut self, draft: DraftId) {
        self.offers.retain(|offer| offer.draft != draft);
        for rearm in &mut self.rearms {
            if rearm.draft == draft {
                rearm.live = false;
                rearm.scope.cancel();
            }
        }
    }
}

impl Brain {
    pub(super) fn issue_rearm(
        &mut self,
        session: IssueSession,
        request: u64,
        draft: DraftId,
        version: StateVersion,
        hash: ContentHash,
        epoch: Option<u64>,
    ) -> Vec<Effect> {
        self.filing.supersede_review(draft);
        if epoch != Some(self.epoch) {
            return self.issue_unavailable_kind(
                self.epoch,
                request,
                draft,
                IssueUnavailableKind::ReviewNotSaved,
            );
        }
        if self.filing.offers.len() + self.filing.rearms.len() >= MAX_ISSUE_REVIEWS {
            return self.issue_unavailable_kind(
                self.epoch,
                request,
                draft,
                IssueUnavailableKind::ReviewCapacity,
            );
        }
        let serial = self.filing.serial();
        let deadline = Instant::now() + FILING_DEADLINE;
        let scope = WriteScope::new(deadline);
        self.filing.rearms.push(Rearm {
            serial,
            epoch: self.epoch,
            session,
            request,
            draft,
            version,
            hash,
            deadline,
            scope: scope.clone(),
            live: true,
        });
        vec![Effect::Store(StoreOp::Issue {
            serial,
            write: Write::Rearm {
                draft,
                at: version,
                hash,
            },
            scope: Some(scope),
        })]
    }

    pub(super) fn issue_review_written(
        &mut self,
        rearm: &Rearm,
        reply: Result<Answer, ()>,
    ) -> Vec<Effect> {
        let current =
            rearm.live && rearm.epoch == self.epoch && self.issue_session() == Some(rearm.session);
        let (action, committed) = match reply {
            Ok(Answer::Changed(action)) => (Some(action), true),
            Ok(Answer::Unchanged(action) | Answer::Refused(_, action)) => (Some(action), false),
            Ok(Answer::Gone) | Err(()) => (None, false),
        };
        let mut effects = Vec::new();
        if let Some(action) = action {
            if committed
                && current
                && !rearm.scope.stopped()
                && action.draft == rearm.draft
                && action.hash() == rearm.hash
                && rearm.version.0.checked_add(1) == Some(action.version.0)
                && matches!(action.stage, Stage::Draft | Stage::Failed { .. })
                && let Some(approval) = action.approval
            {
                self.filing.offers.push(Offered {
                    session: rearm.session,
                    review: rearm.request,
                    draft: action.draft,
                    version: action.version,
                    hash: rearm.hash,
                    approval,
                });
                let state = action.state();
                return vec![Effect::Data(DataToShell::IssueReview {
                    request: rearm.request,
                    card: IssueCard {
                        draft: action.draft,
                        thread: action.thread,
                        repo: action.repo,
                        account: action.account.map(|account| account.login),
                        title: action.title,
                        body: action.body,
                        hash: rearm.hash,
                        approval: Some(approval),
                        version: action.version,
                        state,
                    },
                })];
            }
            effects = self.issue_notice(
                rearm.epoch,
                None,
                rearm.draft,
                &Answer::Unchanged(action),
                None,
            );
        }
        if current {
            effects.extend(self.issue_unavailable_kind(
                rearm.epoch,
                rearm.request,
                rearm.draft,
                IssueUnavailableKind::ReviewNotSaved,
            ));
        }
        effects
    }

    pub(super) fn issue_review_expired(&mut self, serial: u64) -> Vec<Effect> {
        let Some(rearm) = self
            .filing
            .rearms
            .iter_mut()
            .find(|rearm| rearm.serial == serial)
        else {
            return Vec::new();
        };
        if !rearm.live {
            return Vec::new();
        }
        rearm.live = false;
        rearm.scope.cancel();
        let (epoch, request, draft) = (rearm.epoch, rearm.request, rearm.draft);
        self.issue_unavailable_kind(epoch, request, draft, IssueUnavailableKind::ReviewNotSaved)
    }
}
