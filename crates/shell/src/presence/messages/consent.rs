use super::*;
use crate::confirmation;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _};
use kumochi_protocol::message::{
    ApprovalRev, ContentHash, Decision, DraftId, GitHubOp, IssueCard, IssueOp, IssueSession,
    StateVersion,
};
use std::cell::{Cell, Ref};
use std::sync::Arc;

mod presentation;

use presentation::fileable;
pub(super) use presentation::{Labels, Preview, controls};

pub(super) struct Pending {
    draft: DraftId,
    thread: ThreadId,
    version: StateVersion,
    previous_approval: Option<ApprovalRev>,
    hash: ContentHash,
    review: u64,
    scope: u64,
    session: IssueSession,
    generation: u64,
    origin: Arc<()>,
    live: Rc<Cell<bool>>,
    stage: Stage,
}

enum Stage {
    Awaiting,
    Offered {
        version: StateVersion,
        approval: ApprovalRev,
        dialog: confirmation::Handle,
    },
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingIssueConfirmation(..)")
    }
}

impl Messages {
    fn current_issue<'a>(&'a self, draft: DraftId, cx: &'a App) -> Option<Ref<'a, IssueCard>> {
        let model = self.model.read(cx);
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

    fn eligible_issue<'a>(&'a self, draft: DraftId, cx: &'a App) -> Option<Ref<'a, IssueCard>> {
        if self.model.read(cx).issue(draft)?.review_blocked() {
            return None;
        }
        let card = self.current_issue(draft, cx)?;
        fileable(&card, &self.owner, cx).then_some(card)
    }

    fn matches_issue(&self, pending: &Pending, cx: &App) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let owner = owner.read(cx);
        let github = &owner.settings.read(cx).github;
        if !owner.session.as_ref().is_some_and(|session| {
            session.generation() == pending.generation && session.issue_session() == pending.session
        }) || !Arc::ptr_eq(&github.origin, &pending.origin)
            || !github.allows(Some(pending.generation), &GitHubOp::RefreshRepos)
            || github.error.is_some()
        {
            return false;
        }
        let model = self.model.read(cx);
        if !model.loaded
            || model.open != Some(pending.thread)
            || model.issue_scope() != pending.scope
            || model.deleting.contains(&pending.thread)
            || model.issue(pending.draft).is_none()
        {
            return false;
        }
        match &pending.stage {
            Stage::Awaiting => true,
            Stage::Offered {
                version, approval, ..
            } => self.eligible_issue(pending.draft, cx).is_some_and(|card| {
                (card.version, card.hash, card.approval)
                    == (*version, pending.hash, Some(*approval))
            }),
        }
    }

    pub(super) fn dismiss_stale_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending_issue.as_ref().is_some_and(|pending| {
            !pending.live.get()
                || matches!(&pending.stage, Stage::Offered { dialog, .. } if !dialog.live())
                || !self.matches_issue(pending, cx)
        }) {
            self.retire_issue(window, cx);
        }
    }

    pub(super) fn retire_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(pending) = self.pending_issue.take() {
            pending.live.set(false);
            if let Stage::Offered { dialog, .. } = pending.stage {
                dialog.dismiss(window, cx);
            }
            let _gone = self.owner.update(cx, |owner, cx| {
                owner.cancel_issue_review(pending.review, cx);
            });
        }
    }

    pub(in crate::presence) fn retire_review_if(
        &mut self,
        review: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .pending_issue
            .as_ref()
            .is_some_and(|pending| pending.review == review)
        {
            self.retire_issue(window, cx);
        }
    }

    pub(super) fn cancel_waiting_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self
            .pending_issue
            .as_ref()
            .is_some_and(|pending| matches!(&pending.stage, Stage::Awaiting))
        {
            self.retire_issue(window, cx);
            return true;
        }
        false
    }

    pub(super) fn release_issue(&mut self, window: AnyWindowHandle, cx: &mut App) {
        if let Some(pending) = self.pending_issue.take() {
            pending.live.set(false);
            let _gone = self.owner.update(cx, |owner, cx| {
                owner.cancel_issue_review(pending.review, cx);
            });
            if let Stage::Offered { dialog, .. } = pending.stage {
                cx.defer(move |cx| {
                    let _closed = window.update(cx, |_, window, cx| dialog.dismiss(window, cx));
                });
            }
        }
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the confirmation callback supplies a boolean decision"
    )]
    fn decide_issue(
        &mut self,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(pending) = self.pending_issue.take() else {
            return false;
        };
        let offered = match &pending.stage {
            Stage::Offered { approval, .. } => Some(*approval),
            Stage::Awaiting => None,
        };
        if !confirmed
            || !pending.live.get()
            || offered.is_none()
            || !window.is_window_active()
            || !window.is_visible()
            || !self.matches_issue(&pending, cx)
        {
            self.pending_issue = Some(pending);
            self.retire_issue(window, cx);
            return false;
        }
        pending.live.set(false);
        self.owner
            .update(cx, |owner, cx| {
                owner.confirm_issue_review(
                    pending.review,
                    IssueOp::Consent {
                        draft: pending.draft,
                        hash: pending.hash,
                        approval: offered.expect("the offered phase was checked"),
                        decision: Decision::File,
                    },
                    cx,
                )
            })
            .unwrap_or(false)
    }

    fn issue_action(&mut self, op: IssueOp, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !window.is_window_active() || !window.is_visible() {
            return;
        }
        let draft = match op {
            IssueOp::Discard { draft } | IssueOp::CheckAgain { draft } => draft,
            IssueOp::Consent { .. } | IssueOp::Rearm { .. } | IssueOp::CancelReview { .. } => {
                return;
            }
        };
        let Some(card) = self.current_issue(draft, cx) else {
            return;
        };
        if !matches!(
            card.state,
            ActionState::Draft | ActionState::Failed(_) | ActionState::Unknown { .. }
        ) || (matches!(op, IssueOp::CheckAgain { .. })
            && !matches!(card.state, ActionState::Unknown { .. }))
        {
            return;
        }
        drop(card);
        self.retire_issue(window, cx);
        let _gone = self.owner.update(cx, |owner, cx| owner.send_issue(op, cx));
    }

    pub(super) fn open_issue_confirmation(
        &mut self,
        draft: DraftId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.dismiss_stale_issue(window, cx);
        if self.pending_issue.is_some()
            || !window.is_window_active()
            || !window.is_visible()
            || window.has_active_dialog(cx)
        {
            return false;
        }
        let Some(card) = self.eligible_issue(draft, cx) else {
            return false;
        };
        let (thread, version, hash, previous_approval) =
            (card.thread, card.version, card.hash, card.approval);
        drop(card);
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let owner = owner.read(cx);
        let generation = owner
            .session
            .as_ref()
            .expect("eligibility checked the session")
            .generation();
        let session = owner
            .session
            .as_ref()
            .expect("eligibility checked the session")
            .issue_session();
        let origin = Arc::clone(&owner.settings.read(cx).github.origin);
        let scope = self.model.read(cx).issue_scope();
        let live = Rc::new(Cell::new(true));
        let view = cx.weak_entity();
        let review = self
            .owner
            .update(cx, |owner, cx| {
                owner.begin_issue_review(
                    IssueOp::Rearm {
                        draft,
                        version,
                        hash,
                    },
                    view,
                    window.window_handle(),
                    Rc::clone(&live),
                    cx,
                )
            })
            .ok()
            .flatten();
        let Some(review) = review else {
            return false;
        };
        self.pending_issue = Some(Pending {
            draft,
            thread,
            version,
            previous_approval,
            hash,
            review,
            scope,
            session,
            generation,
            origin,
            live,
            stage: Stage::Awaiting,
        });
        true
    }

    pub(in crate::presence) fn receive_issue_review(
        &mut self,
        request: u64,
        card: IssueCard,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(mut pending) = self.pending_issue.take() else {
            return false;
        };
        if pending.review != request || !matches!(&pending.stage, Stage::Awaiting) {
            self.pending_issue = Some(pending);
            return false;
        }
        if !pending.live.get()
            || !window.is_window_active()
            || !window.is_visible()
            || !self.matches_issue(&pending, cx)
            || card.draft != pending.draft
            || card.thread != pending.thread
            || card.hash != pending.hash
            || card.version <= pending.version
            || card.approval.is_none()
            || card.approval == pending.previous_approval
            || !fileable(&card, &self.owner, cx)
        {
            self.pending_issue = Some(pending);
            self.retire_issue(window, cx);
            return false;
        }
        let strings = cx.global::<Strings>();
        let preview = Preview::new(&card, cx);
        let heading = strings.get(Text::IssueConsentTitle).into();
        let cancel = strings.get(Text::IssueConsentCancel).into();
        let confirm = strings.get(Text::IssueConsentConfirm).into();
        let version = card.version;
        let approval = card.approval.expect("the offered approval was checked");
        let accepted = self
            .owner
            .update(cx, |owner, cx| owner.accept_issue_review(request, card, cx))
            .unwrap_or(false);
        if !accepted {
            self.pending_issue = Some(pending);
            self.retire_issue(window, cx);
            return false;
        }
        let preview = cx.new(|_| preview);
        let view = cx.weak_entity();
        let Some(dialog) = confirmation::open_content(
            window,
            cx,
            heading,
            move |alert, _, _| {
                let preview = preview.clone();
                alert.content(move |content, _, _| content.min_h_0().child(preview.clone()))
            },
            cancel,
            confirm,
            move |confirmed, window, cx| {
                let _gone = view.update(cx, |view, cx| {
                    let _queued = view.decide_issue(confirmed, window, cx);
                });
            },
        ) else {
            self.pending_issue = Some(pending);
            self.retire_issue(window, cx);
            return false;
        };
        self.model.update(cx, |model, _| {
            model.begin_issue_confirmation(pending.draft, &pending.live);
        });
        pending.stage = Stage::Offered {
            version,
            approval,
            dialog,
        };
        self.pending_issue = Some(pending);
        true
    }
}
