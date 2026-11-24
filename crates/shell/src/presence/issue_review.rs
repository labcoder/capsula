use super::{Presence, messages::Messages};
use gpui_kit::{AnyWindowHandle, Context, WeakEntity};
use kumochi_protocol::message::{DraftId, IssueCard, IssueOp, IssueUnavailableKind};
use std::cell::Cell;
use std::rc::Rc;

use kumochi_shell_core::presence::issue_review::machine;

use machine::{Effect, Reviews};

type Target = (WeakEntity<Messages>, AnyWindowHandle);
pub(in crate::presence) type Route = machine::Route<Target>;

impl Presence {
    /// Takes one review decision on the owner's state, applying each effect where it arises.
    fn review<R>(
        &mut self,
        cx: &mut Context<'_, Self>,
        decide: impl FnOnce(&mut Reviews<'_, Target>) -> R,
    ) -> R {
        let (threads, chat) = (&self.threads, &self.chat);
        let mut apply = |effect: Effect<Target>| match effect {
            Effect::Threads(update) => {
                threads.update(cx, |model, cx| {
                    update.apply(model);
                    cx.notify();
                });
                chat.update(cx, |_, cx| cx.notify());
            }
            Effect::RetireView((view, window), request) => cx.defer(move |cx| {
                let _gone = window.update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.retire_review_if(request, window, cx);
                    })
                });
            }),
        };
        decide(&mut Reviews {
            route: &mut self.issue_review,
            request: &mut self.issue_request,
            session: self.session.as_ref(),
            link: &self.link,
            apply: &mut apply,
        })
    }

    pub(super) fn begin_issue_review(
        &mut self,
        op: IssueOp,
        view: WeakEntity<Messages>,
        window: AnyWindowHandle,
        live: Rc<Cell<bool>>,
        cx: &mut Context<'_, Self>,
    ) -> Option<u64> {
        self.review(cx, |reviews| reviews.begin(op, (view, window), live))
    }

    pub(in crate::presence) fn retire_issue_review(&mut self, cx: &mut Context<'_, Self>) {
        self.review(cx, |reviews| {
            reviews.retire();
        });
    }

    pub(super) fn cancel_issue_review(&mut self, review: u64, cx: &mut Context<'_, Self>) {
        self.review(cx, |reviews| reviews.cancel_review(review));
    }

    pub(in crate::presence) fn fail_issue_review(
        &mut self,
        request: u64,
        draft: DraftId,
        cx: &mut Context<'_, Self>,
    ) {
        self.review(cx, |reviews| reviews.fail(request, draft));
    }

    pub(in crate::presence) fn issue_unavailable(
        &mut self,
        request: u64,
        draft: DraftId,
        kind: IssueUnavailableKind,
        cx: &mut Context<'_, Self>,
    ) {
        self.review(cx, |reviews| reviews.unavailable(request, draft, kind));
    }

    pub(super) fn confirm_issue_review(
        &mut self,
        review: u64,
        op: IssueOp,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.review(cx, |reviews| reviews.confirm(review, op))
    }

    pub(super) fn accept_issue_review(
        &mut self,
        request: u64,
        card: IssueCard,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.review(cx, |reviews| reviews.accepts(request)) {
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
        let Some(((view, window), session, live)) =
            self.review(cx, |reviews| reviews.deliver(request, card.draft))
        else {
            return false;
        };
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
        self.review(cx, |reviews| reviews.send(op))
    }
}
