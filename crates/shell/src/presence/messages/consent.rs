use super::*;
use crate::confirmation;
use gpui_kit::{AppContext as _, ScrollHandle, StatefulInteractiveElement as _};
use kumochi_protocol::message::{
    ApprovalRev, ContentHash, ControlToBrain, Decision, DraftId, GitHubOp, GitHubView, IssueCard,
    IssueOp, StateVersion,
};
use std::cell::{Cell, Ref};
use std::sync::Arc;

pub(super) struct Pending {
    draft: DraftId,
    thread: ThreadId,
    version: StateVersion,
    hash: ContentHash,
    approval: ApprovalRev,
    generation: u64,
    origin: Arc<()>,
    live: Rc<Cell<bool>>,
    dialog: confirmation::Handle,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingIssueConfirmation(..)")
    }
}

pub(super) struct Preview {
    title: SharedString,
    details: SharedString,
    consequence: SharedString,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuePreview")
            .field("title_len", &self.title.len())
            .field("details_len", &self.details.len())
            .finish_non_exhaustive()
    }
}

impl Preview {
    pub(super) fn new(card: &IssueCard, cx: &App) -> Self {
        let strings = cx.global::<Strings>();
        Self {
            title: strings.issue_title(card.title.as_str()).into(),
            details: strings.issue_details(card).into(),
            consequence: strings.get(Text::IssueConsentConsequence).into(),
            focus: cx.focus_handle().tab_stop(true),
            scroll: ScrollHandle::new(),
        }
    }

    pub(super) fn texts(&self) -> [gpui_kit::Text; 3] {
        [
            gpui_kit::Text::new("issue-preview-title".into(), self.title.clone()),
            gpui_kit::Text::new("issue-preview-details".into(), self.details.clone()),
            gpui_kit::Text::new("issue-preview-consequence".into(), self.consequence.clone()),
        ]
    }
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .relative()
            .h(window.rem_size() * 18.0)
            .min_h_0()
            .w_full()
            .when(self.focus.is_focused(window), |this| {
                this.focus_ring_style(window, cx)
            })
            .child(
                div()
                    .id("issue-preview-scroll")
                    .debug_selector(|| "issue-preview-scroll".into())
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .track_focus(&self.focus)
                    .key_context("IssuePreview")
                    .on_key_down(cx.listener(|view, event: &gpui_kit::KeyDownEvent, _, cx| {
                        let mut offset = view.scroll.offset();
                        let page = view.scroll.bounds().size.height * 0.9;
                        match event.keystroke.key.as_str() {
                            "pageup" => offset.y += page,
                            "pagedown" => offset.y -= page,
                            "up" => offset.y += px(24.0),
                            "down" => offset.y -= px(24.0),
                            "home" => offset.y = px(0.0),
                            "end" => offset.y = -view.scroll.max_offset().y,
                            _ => return,
                        }
                        offset.y = offset.y.clamp(-view.scroll.max_offset().y, px(0.0));
                        view.scroll.set_offset(offset);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(
                        div()
                            .debug_selector(|| "issue-preview-fields".into())
                            .flex()
                            .flex_col()
                            .gap_3()
                            .text_sm()
                            .children(self.texts()),
                    ),
            )
            .child(Scrollbar::vertical(&self.scroll))
    }
}

impl Messages {
    fn eligible_issue<'a>(&'a self, draft: DraftId, cx: &'a App) -> Option<Ref<'a, IssueCard>> {
        let owner = self.owner.upgrade()?;
        let owner = owner.read(cx);
        let generation = owner.session.as_ref()?.generation();
        let github = &owner.settings.read(cx).github;
        if github.generation != Some(generation)
            || github.error.is_some()
            || !github.allows(Some(generation), &GitHubOp::RefreshRepos)
        {
            return None;
        }
        let (login, repos) = match github.view.as_ref()? {
            GitHubView::Connected { login, repos, .. } => (login, repos),
            GitHubView::SignedOut { .. }
            | GitHubView::Requesting
            | GitHubView::AwaitingUser { .. }
            | GitHubView::NotInstalled { .. }
            | GitHubView::NeedsSignIn { .. }
            | GitHubView::NotVerified { .. } => return None,
        };
        let model = self.model.read(cx);
        let entry = model.issue(draft)?;
        if entry.loading() || !model.loaded {
            return None;
        }
        let card = entry.card()?;
        if model.open != Some(card.thread)
            || model.deleting.contains(&card.thread)
            || card.approval.is_none()
            || !matches!(card.state, ActionState::Draft | ActionState::Failed(_))
            || card.account.as_ref() != Some(login)
            || !card.repo.as_ref().is_some_and(|repo| repos.contains(repo))
        {
            return None;
        }
        Some(Ref::map(entry, |entry| {
            entry.card().expect("the full card was checked")
        }))
    }

    fn matches_issue(&self, pending: &Pending, cx: &App) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let owner = owner.read(cx);
        let github = &owner.settings.read(cx).github;
        owner.session.as_ref().map(crate::link::Session::generation) == Some(pending.generation)
            && Arc::ptr_eq(&github.origin, &pending.origin)
            && self.eligible_issue(pending.draft, cx).is_some_and(|card| {
                (card.thread, card.version, card.hash, card.approval)
                    == (
                        pending.thread,
                        pending.version,
                        pending.hash,
                        Some(pending.approval),
                    )
            })
    }

    pub(super) fn dismiss_stale_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending_issue.as_ref().is_some_and(|pending| {
            !pending.live.get() || !pending.dialog.live() || !self.matches_issue(pending, cx)
        }) {
            self.retire_issue(window, cx);
        }
    }

    pub(super) fn retire_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(pending) = self.pending_issue.take() {
            pending.live.set(false);
            pending.dialog.dismiss(window, cx);
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
        let live = pending.live.replace(false);
        if !confirmed
            || !live
            || !window.is_window_active()
            || !window.is_visible()
            || !self.matches_issue(&pending, cx)
        {
            return false;
        }
        self.owner
            .update(cx, |owner, _| {
                owner.session.as_ref().is_some_and(|session| {
                    session.generation() == pending.generation
                        && owner.link.send_control(
                            session,
                            ControlToBrain::Issue(IssueOp::Consent {
                                draft: pending.draft,
                                hash: pending.hash,
                                approval: pending.approval,
                                decision: Decision::File,
                            }),
                        )
                })
            })
            .unwrap_or(false)
    }

    pub(super) fn open_issue_confirmation(
        &mut self,
        draft: DraftId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.dismiss_stale_issue(window, cx);
        if self.pending_issue.is_some() || !window.is_window_active() || !window.is_visible() {
            return false;
        }
        let Some(card) = self.eligible_issue(draft, cx) else {
            return false;
        };
        let (thread, version, hash, approval) = (
            card.thread,
            card.version,
            card.hash,
            card.approval.expect("eligible cards have an approval"),
        );
        let strings = cx.global::<Strings>();
        let preview = Preview::new(&card, cx);
        let heading = strings.get(Text::IssueConsentTitle).into();
        let cancel = strings.get(Text::IssueConsentCancel).into();
        let confirm = strings.get(Text::IssueConsentConfirm).into();
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
        let origin = Arc::clone(&owner.settings.read(cx).github.origin);
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
            return false;
        };
        let live = Rc::new(Cell::new(true));
        self.model
            .update(cx, |model, _| model.begin_issue_confirmation(draft, &live));
        self.pending_issue = Some(Pending {
            draft,
            thread,
            version,
            hash,
            approval,
            generation,
            origin,
            live,
            dialog,
        });
        true
    }
}
