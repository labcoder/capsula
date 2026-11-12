use super::{Presence, messages::Messages};
use gpui_kit::{AnyWindowHandle, Context, WeakEntity};
use kumochi_protocol::message::{ControlToBrain, DraftId, IssueCard, IssueOp};
use std::cell::Cell;
use std::rc::Rc;

pub(in crate::presence) struct Route {
    session: crate::link::Session,
    request: u64,
    draft: DraftId,
    view: WeakEntity<Messages>,
    window: AnyWindowHandle,
    live: Rc<Cell<bool>>,
    delivered: bool,
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
                    op: Box::new(op),
                },
            )
            .then_some(request)
    }

    pub(super) fn begin_issue_review(
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

    pub(super) fn cancel_issue_review(&mut self, review: u64, cx: &mut Context<'_, Self>) {
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

    pub(super) fn confirm_issue_review(
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

    pub(super) fn accept_issue_review(
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

    pub(super) fn send_issue(&mut self, op: IssueOp, cx: &mut Context<'_, Self>) -> bool {
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
