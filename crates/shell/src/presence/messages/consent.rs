use super::*;
use crate::confirmation;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::{AnyWindowHandle, AppContext as _, ScrollHandle, StatefulInteractiveElement as _};
use kumochi_protocol::message::{
    ApprovalRev, ContentHash, ControlToBrain, Decision, DraftId, GitHubOp, GitHubView, IssueCard,
    IssueOp, IssueSession, StateVersion,
};
use std::cell::{Cell, Ref};
use std::sync::Arc;

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

pub(in crate::presence) struct Route {
    session: crate::link::Session,
    request: u64,
    draft: DraftId,
    view: WeakEntity<Messages>,
    window: AnyWindowHandle,
    live: Rc<Cell<bool>>,
    delivered: bool,
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

    fn retire_review_if(&mut self, review: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
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

    fn receive_issue_review(
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

fn fileable(card: &IssueCard, owner: &WeakEntity<Presence>, cx: &App) -> bool {
    matches!(card.state, ActionState::Draft | ActionState::Failed(_))
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
    let file = fileable(card, owner, cx) && !entry.review_blocked();
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
                    .disabled(entry.pending_action())
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
        if let Some(review) = entry.pending_review_request() {
            let owner = owner.clone();
            buttons = buttons.child(
                Button::new(("issue-cancel-review", draft.0))
                    .debug_selector(move || format!("issue-cancel-review-{}", draft.0))
                    .label(strings.get(Text::IssueConsentCancel))
                    .outline()
                    .on_click(move |_, window, cx| {
                        if window.is_window_active() && window.is_visible() {
                            let _gone =
                                owner.update(cx, |owner, cx| owner.cancel_issue_review(review, cx));
                        }
                    }),
            );
        }
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
            && !entry.review_blocked()
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
    fn issue_message(&mut self, session: &crate::link::Session, op: IssueOp) -> Option<u64> {
        let request = self.issue_request.checked_add(1)?;
        self.issue_request = request;
        self.link
            .send_control(
                session,
                ControlToBrain::Issue {
                    session: session.issue_session(),
                    request,
                    op,
                },
            )
            .then_some(request)
    }

    fn begin_issue_review(
        &mut self,
        op: IssueOp,
        view: WeakEntity<Messages>,
        window: AnyWindowHandle,
        live: Rc<Cell<bool>>,
        cx: &mut Context<'_, Self>,
    ) -> Option<u64> {
        let IssueOp::Rearm { draft, .. } = op else {
            return None;
        };
        self.retire_issue_review(cx);
        let session = self.session.clone()?;
        let Some(request) = self.issue_message(&session, op) else {
            self.threads.update(cx, |model, cx| {
                model.issue_submitted(draft, None);
                cx.notify();
            });
            self.chat.update(cx, |_, cx| cx.notify());
            self.link.end(&session);
            return None;
        };
        self.issue_review = Some(Route {
            session,
            request,
            draft,
            view,
            window,
            live,
            delivered: false,
        });
        self.threads.update(cx, |model, cx| {
            model.issue_review_started(draft, request);
            cx.notify();
        });
        self.chat.update(cx, |_, cx| cx.notify());
        Some(request)
    }

    fn cancel_review_route(&mut self, route: Route, cx: &mut Context<'_, Self>) {
        route.live.set(false);
        if self.session.as_ref().is_some_and(|session| {
            session.generation() == route.session.generation()
                && session.issue_session() == route.session.issue_session()
        }) && self
            .issue_message(
                &route.session,
                IssueOp::CancelReview {
                    draft: route.draft,
                    review: route.request,
                },
            )
            .is_none()
        {
            self.link.end(&route.session);
        }
        self.threads.update(cx, |model, cx| {
            model.cancel_issue_review(route.request, route.draft);
            cx.notify();
        });
        self.chat.update(cx, |_, cx| cx.notify());
        cx.defer(move |cx| {
            let _gone = route.window.update(cx, |_, window, cx| {
                route.view.update(cx, |view, cx| {
                    view.retire_review_if(route.request, window, cx);
                })
            });
        });
    }

    pub(in crate::presence) fn retire_issue_review(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(route) = self.issue_review.take() {
            self.cancel_review_route(route, cx);
        }
    }

    fn cancel_issue_review(&mut self, review: u64, cx: &mut Context<'_, Self>) {
        if self
            .issue_review
            .as_ref()
            .is_some_and(|route| route.request == review)
        {
            self.retire_issue_review(cx);
        }
    }

    pub(in crate::presence) fn fail_issue_review(
        &mut self,
        request: u64,
        draft: DraftId,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .issue_review
            .as_ref()
            .is_some_and(|route| route.request == request && route.draft == draft)
        {
            self.retire_issue_review(cx);
        }
    }

    fn confirm_issue_review(
        &mut self,
        review: u64,
        op: IssueOp,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self
            .issue_review
            .as_ref()
            .is_none_or(|route| route.request != review || !route.delivered)
        {
            return false;
        }
        let route = self
            .issue_review
            .take()
            .expect("the current offered route was checked");
        route.live.set(false);
        let queued = self.send_issue(op, cx);
        if !queued {
            self.cancel_review_route(route, cx);
        }
        queued
    }

    fn accept_issue_review(
        &mut self,
        request: u64,
        card: IssueCard,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self
            .issue_review
            .as_ref()
            .is_none_or(|route| route.request != request || !route.live.get() || !route.delivered)
        {
            return false;
        }
        let accepted = self.threads.update(cx, |model, cx| {
            let accepted = model.issue_review(request, card);
            cx.notify();
            accepted
        });
        self.chat.update(cx, |_, cx| cx.notify());
        accepted
    }

    pub(in crate::presence) fn deliver_issue_review(
        &mut self,
        request: u64,
        card: IssueCard,
        bytes: u32,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(route) = self.issue_review.as_mut().filter(|route| {
            route.request == request
                && route.draft == card.draft
                && route.live.get()
                && !route.delivered
        }) else {
            return false;
        };
        route.delivered = true;
        let view = route.view.clone();
        let window = route.window;
        let session = route.session.clone();
        let live = Rc::clone(&route.live);
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let accepted = live.get()
                && window
                    .update(cx, |_, window, cx| {
                        view.update(cx, |view, cx| {
                            view.receive_issue_review(request, card, window, cx)
                        })
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
            if !accepted {
                let _gone = owner.update(cx, |owner, cx| owner.cancel_issue_review(request, cx));
            }
            session.grant(bytes);
        });
        true
    }

    fn send_issue(&mut self, op: IssueOp, cx: &mut Context<'_, Self>) -> bool {
        let draft = match op {
            IssueOp::Consent { draft, .. }
            | IssueOp::Discard { draft }
            | IssueOp::CheckAgain { draft } => draft,
            IssueOp::Rearm { .. } | IssueOp::CancelReview { .. } => return false,
        };
        self.retire_issue_review(cx);
        let request = self
            .session
            .clone()
            .and_then(|session| self.issue_message(&session, op));
        self.threads.update(cx, |model, cx| {
            model.issue_submitted(draft, request);
            cx.notify();
        });
        self.chat.update(cx, |_, cx| cx.notify());
        request.is_some()
    }
}
