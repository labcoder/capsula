use crate::link::Session;
use crate::settings::Settings;
use crate::threads::Threads;
use kumochi_protocol::message::{
    ActionState, ApprovalRev, ContentHash, Decision, DraftId, GitHubOp, GitHubView, IssueCard,
    IssueOp, IssueSession, StateVersion, ThreadId,
};
use std::cell::{Cell, Ref};
use std::rc::Rc;
use std::sync::Arc;

/// What the consent decisions read: the view's threads and the owner's connection, if any.
pub(super) struct Facts<'a> {
    pub(super) model: &'a Threads,
    pub(super) connection: Option<(&'a Session, &'a Settings)>,
}

/// The view's window as the decision sees it.
pub(super) struct Window {
    pub(super) active: bool,
    pub(super) visible: bool,
    pub(super) dialog: bool,
}

/// A confirmation that may open: what it rearms and the intent it becomes once a review exists.
pub(super) struct Opening {
    draft: DraftId,
    thread: ThreadId,
    version: StateVersion,
    previous_approval: Option<ApprovalRev>,
    hash: ContentHash,
    scope: u64,
    session: IssueSession,
    generation: u64,
    origin: Arc<()>,
    live: Rc<Cell<bool>>,
}

pub(in crate::presence) struct Pending<D> {
    opening: Opening,
    review: u64,
    stage: Stage<D>,
}

enum Stage<D> {
    Awaiting,
    Offered {
        version: StateVersion,
        approval: ApprovalRev,
        dialog: D,
    },
}

impl<D> std::fmt::Debug for Pending<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingIssueConfirmation(..)")
    }
}

fn current(model: &Threads, draft: DraftId) -> Option<Ref<'_, IssueCard>> {
    let entry = model.issue(draft)?;
    if entry.loading() || entry.pending_action() || !model.loaded {
        return None;
    }
    let card = entry.card()?;
    if model.open != Some(card.thread) || model.deleting.contains(&card.thread) {
        return None;
    }
    Some(Ref::map(entry, |entry| {
        entry.card().expect("the full card was checked")
    }))
}

fn eligible<'a>(facts: &Facts<'a>, draft: DraftId) -> Option<Ref<'a, IssueCard>> {
    if facts.model.issue(draft)?.review_blocked() {
        return None;
    }
    let card = current(facts.model, draft)?;
    fileable(&card, facts.connection).then_some(card)
}

pub(super) fn fileable(card: &IssueCard, connection: Option<(&Session, &Settings)>) -> bool {
    matches!(card.state, ActionState::Draft | ActionState::Failed(_))
        && bound_to_connection(card, connection)
}

pub(super) fn bound_to_connection(
    card: &IssueCard,
    connection: Option<(&Session, &Settings)>,
) -> bool {
    let Some((session, settings)) = connection else {
        return false;
    };
    let generation = session.generation();
    let github = &settings.github;
    github.generation == Some(generation)
        && github.error.is_none()
        && github.allows(Some(generation), &GitHubOp::RefreshRepos)
        && matches!(github.view.as_ref(), Some(GitHubView::Connected { login, repos, .. })
            if card.account.as_ref() == Some(login)
                && card.repo.as_ref().is_some_and(|repo| repos.contains(repo)))
}

/// Discard and check again apply only to a current card in a state that offers them.
pub(super) fn action_allowed(op: &IssueOp, window: &Window, model: &Threads) -> bool {
    if !window.active || !window.visible {
        return false;
    }
    let draft = match op {
        IssueOp::Discard { draft } | IssueOp::CheckAgain { draft } => *draft,
        IssueOp::Consent { .. } | IssueOp::Rearm { .. } | IssueOp::CancelReview { .. } => {
            return false;
        }
    };
    let Some(card) = current(model, draft) else {
        return false;
    };
    if !matches!(
        card.state,
        ActionState::Draft | ActionState::Failed(_) | ActionState::Unknown { .. }
    ) || (matches!(op, IssueOp::CheckAgain { .. })
        && !matches!(card.state, ActionState::Unknown { .. }))
    {
        return false;
    }
    true
}

impl Opening {
    pub(super) fn new<D>(
        pending: Option<&Pending<D>>,
        draft: DraftId,
        window: &Window,
        facts: &Facts<'_>,
    ) -> Option<Self> {
        if pending.is_some() || !window.active || !window.visible || window.dialog {
            return None;
        }
        let card = eligible(facts, draft)?;
        let (session, settings) = facts.connection.expect("eligibility checked the session");
        Some(Self {
            draft,
            thread: card.thread,
            version: card.version,
            previous_approval: card.approval,
            hash: card.hash,
            scope: facts.model.issue_scope(),
            session: session.issue_session(),
            generation: session.generation(),
            origin: Arc::clone(&settings.github.origin),
            live: Rc::new(Cell::new(true)),
        })
    }

    pub(super) fn rearm(&self) -> IssueOp {
        IssueOp::Rearm {
            draft: self.draft,
            version: self.version,
            hash: self.hash,
        }
    }

    pub(super) fn live(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.live)
    }

    pub(super) fn pending<D>(self, review: u64) -> Pending<D> {
        Pending {
            opening: self,
            review,
            stage: Stage::Awaiting,
        }
    }
}

impl<D> Pending<D> {
    pub(super) fn review(&self) -> u64 {
        self.review
    }

    pub(super) fn awaiting(&self) -> bool {
        matches!(self.stage, Stage::Awaiting)
    }

    fn matches(&self, facts: &Facts<'_>) -> bool {
        let pending = &self.opening;
        let Some((session, settings)) = facts.connection else {
            return false;
        };
        let github = &settings.github;
        if session.generation() != pending.generation
            || session.issue_session() != pending.session
            || !Arc::ptr_eq(&github.origin, &pending.origin)
            || !github.allows(Some(pending.generation), &GitHubOp::RefreshRepos)
            || github.error.is_some()
        {
            return false;
        }
        let model = facts.model;
        if !model.loaded
            || model.open != Some(pending.thread)
            || model.issue_scope() != pending.scope
            || model.deleting.contains(&pending.thread)
            || model.issue(pending.draft).is_none()
        {
            return false;
        }
        match &self.stage {
            Stage::Awaiting => true,
            Stage::Offered {
                version, approval, ..
            } => eligible(facts, pending.draft).is_some_and(|card| {
                (card.version, card.hash, card.approval)
                    == (*version, pending.hash, Some(*approval))
            }),
        }
    }

    /// No longer live, its dialog gone, or the session, scope or card it was offered for moved on.
    pub(super) fn stale(&self, facts: &Facts<'_>, dialog_live: impl Fn(&D) -> bool) -> bool {
        !self.opening.live.get()
            || matches!(&self.stage, Stage::Offered { dialog, .. } if !dialog_live(dialog))
            || !self.matches(facts)
    }

    /// Ends this intent: the review to cancel and the dialog to dismiss, if one was offered.
    pub(super) fn retire(self) -> (u64, Option<D>) {
        self.opening.live.set(false);
        let dialog = match self.stage {
            Stage::Offered { dialog, .. } => Some(dialog),
            Stage::Awaiting => None,
        };
        (self.review, dialog)
    }

    /// The consent to file for a confirmed, still current offer, which ends this intent.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the confirmation callback supplies a boolean decision"
    )]
    pub(super) fn consent(
        &self,
        confirmed: bool,
        window: &Window,
        facts: &Facts<'_>,
    ) -> Option<(u64, IssueOp)> {
        let Stage::Offered { approval, .. } = self.stage else {
            return None;
        };
        if !confirmed
            || !self.opening.live.get()
            || !window.active
            || !window.visible
            || !self.matches(facts)
        {
            return None;
        }
        self.opening.live.set(false);
        Some((
            self.review,
            IssueOp::Consent {
                draft: self.opening.draft,
                hash: self.opening.hash,
                approval,
                decision: Decision::File,
            },
        ))
    }

    /// The reply this intent waits for.
    pub(super) fn awaits(&self, request: u64) -> bool {
        self.review == request && self.awaiting()
    }

    /// The reply can't be offered: the intent or window moved on, or the card isn't the newer
    /// approval of the same fileable draft.
    pub(super) fn refuses(&self, card: &IssueCard, window: &Window, facts: &Facts<'_>) -> bool {
        let pending = &self.opening;
        !pending.live.get()
            || !window.active
            || !window.visible
            || !self.matches(facts)
            || card.draft != pending.draft
            || card.thread != pending.thread
            || card.hash != pending.hash
            || card.version <= pending.version
            || card.approval.is_none()
            || card.approval == pending.previous_approval
            || !fileable(card, facts.connection)
    }

    pub(super) fn offer(
        &mut self,
        model: &mut Threads,
        version: StateVersion,
        approval: ApprovalRev,
        dialog: D,
    ) {
        model.begin_issue_confirmation(self.opening.draft, &self.opening.live);
        self.stage = Stage::Offered {
            version,
            approval,
            dialog,
        };
    }
}
