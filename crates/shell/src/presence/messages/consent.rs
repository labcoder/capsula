use super::*;
use crate::confirmation;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
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
    fn current_issue<'a>(&'a self, draft: DraftId, cx: &'a App) -> Option<Ref<'a, IssueCard>> {
        let model = self.model.read(cx);
        let entry = model.issue(draft)?;
        if entry.loading() || entry.pending() || !model.loaded {
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
        let card = self.current_issue(draft, cx)?;
        fileable(&card, &self.owner, cx).then_some(card)
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
            .update(cx, |owner, cx| {
                owner.send_issue(
                    IssueOp::Consent {
                        draft: pending.draft,
                        hash: pending.hash,
                        approval: pending.approval,
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
            IssueOp::Consent { .. } => return,
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

fn fileable(card: &IssueCard, owner: &WeakEntity<Presence>, cx: &App) -> bool {
    card.approval.is_some()
        && matches!(card.state, ActionState::Draft | ActionState::Failed(_))
        && bound_to_connection(card, owner, cx)
}

fn bound_to_connection(card: &IssueCard, owner: &WeakEntity<Presence>, cx: &App) -> bool {
    let Some(owner) = owner.upgrade() else {
        return false;
    };
    let owner = owner.read(cx);
    let Some(generation) = owner.session.as_ref().map(crate::link::Session::generation) else {
        return false;
    };
    let github = &owner.settings.read(cx).github;
    github.generation == Some(generation)
        && github.error.is_none()
        && github.allows(Some(generation), &GitHubOp::RefreshRepos)
        && matches!(github.view.as_ref(), Some(GitHubView::Connected { login, repos, .. })
            if card.account.as_ref() == Some(login)
                && card.repo.as_ref().is_some_and(|repo| repos.contains(repo)))
}

pub(super) fn controls(
    draft: DraftId,
    model: &Threads,
    owner: &WeakEntity<Presence>,
    view: &WeakEntity<Messages>,
    cx: &App,
) -> gpui_kit::AnyElement {
    let Some(entry) = model.issue(draft) else {
        return div().into_any_element();
    };
    let Some(card) = entry.card().filter(|card| {
        model.loaded
            && model.open == Some(card.thread)
            && !model.deleting.contains(&card.thread)
            && !entry.loading()
    }) else {
        return div().into_any_element();
    };
    let actionable = matches!(
        card.state,
        ActionState::Draft | ActionState::Failed(_) | ActionState::Unknown { .. }
    );
    let file = fileable(card, owner, cx);
    let pending = entry.pending();
    let strings = cx.global::<Strings>();
    let mut controls = div().flex().flex_col().gap_2();
    if actionable {
        let mut buttons = div().flex().gap_2();
        if file {
            let view = view.clone();
            buttons = buttons.child(
                Button::new(("issue-file", draft.0))
                    .debug_selector(move || format!("issue-file-{}", draft.0))
                    .label(strings.get(Text::IssueConsentOpen))
                    .disabled(pending)
                    .on_click(move |_, window, cx| {
                        let _gone = view.update(cx, |view, cx| {
                            view.open_issue_confirmation(draft, window, cx)
                        });
                    }),
            );
        }
        if matches!(card.state, ActionState::Unknown { .. }) {
            let view = view.clone();
            buttons = buttons.child(
                Button::new(("issue-check", draft.0))
                    .debug_selector(move || format!("issue-check-{}", draft.0))
                    .label(strings.get(Text::IssueCheckAgain))
                    .outline()
                    .disabled(pending)
                    .on_click(move |_, window, cx| {
                        let _gone = view.update(cx, |view, cx| {
                            view.issue_action(IssueOp::CheckAgain { draft }, window, cx);
                        });
                    }),
            );
        }
        let view = view.clone();
        buttons = buttons.child(
            Button::new(("issue-dismiss", draft.0))
                .debug_selector(move || format!("issue-dismiss-{}", draft.0))
                .label(strings.get(Text::IssueDismiss))
                .ghost()
                .disabled(pending)
                .on_click(move |_, window, cx| {
                    let _gone = view.update(cx, |view, cx| {
                        view.issue_action(IssueOp::Discard { draft }, window, cx);
                    });
                }),
        );
        controls = controls.child(buttons);
        if !pending
            && ((!file && !matches!(card.state, ActionState::Unknown { .. }))
                || !bound_to_connection(card, owner, cx))
        {
            controls = controls.child(div().debug_selector(|| "issue-replacement".into()).child(
                gpui_kit::Text::new(
                    "issue-replacement".into(),
                    strings.get(Text::IssueReplacement).into(),
                ),
            ));
        }
    }
    controls.into_any_element()
}

impl Presence {
    fn send_issue(&mut self, op: IssueOp, cx: &mut Context<'_, Self>) -> bool {
        let draft = match op {
            IssueOp::Consent { draft, .. }
            | IssueOp::Discard { draft }
            | IssueOp::CheckAgain { draft } => draft,
        };
        let request = self.issue_request.checked_add(1);
        let queued = request.is_some_and(|request| {
            self.issue_request = request;
            self.session.as_ref().is_some_and(|session| {
                self.link
                    .send_control(session, ControlToBrain::Issue { request, op })
            })
        });
        self.threads.update(cx, |model, cx| {
            model.issue_submitted(draft, request.filter(|_| queued));
            cx.notify();
        });
        self.chat.update(cx, |_, cx| cx.notify());
        queued
    }
}
