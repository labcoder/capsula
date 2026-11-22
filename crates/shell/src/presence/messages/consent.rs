use super::*;
use crate::confirmation;
use crate::link::Session;
use crate::settings::Settings;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _};
use kumochi_protocol::message::{DraftId, IssueCard, IssueOp};

use kumochi_shell_core::presence::messages::consent::machine;
mod presentation;

pub(super) use presentation::{Labels, Preview, controls};

pub(super) type Pending = machine::Pending<confirmation::Handle>;

fn connection<'a>(
    owner: &WeakEntity<Presence>,
    cx: &'a App,
) -> Option<(&'a Session, &'a Settings)> {
    let owner = owner.upgrade()?.read(cx);
    Some((owner.session.as_ref()?, owner.settings.read(cx)))
}

fn window_state(window: &mut Window, cx: &mut App) -> machine::Window {
    machine::Window {
        active: window.is_window_active(),
        visible: window.is_visible(),
        dialog: window.has_active_dialog(cx),
    }
}

impl Messages {
    fn facts<'a>(&self, cx: &'a App) -> machine::Facts<'a> {
        machine::Facts {
            model: self.model.read(cx),
            connection: connection(&self.owner, cx),
        }
    }

    pub(super) fn dismiss_stale_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self
            .pending_issue
            .as_ref()
            .is_some_and(|pending| pending.stale(&self.facts(cx), confirmation::Handle::live))
        {
            self.retire_issue(window, cx);
        }
    }

    pub(super) fn retire_issue(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(pending) = self.pending_issue.take() {
            let (review, dialog) = pending.retire();
            if let Some(dialog) = dialog {
                dialog.dismiss(window, cx);
            }
            let _gone = self.owner.update(cx, |owner, cx| {
                owner.cancel_issue_review(review, cx);
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
            .is_some_and(|pending| pending.review() == review)
        {
            self.retire_issue(window, cx);
        }
    }

    pub(super) fn cancel_waiting_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.pending_issue.as_ref().is_some_and(Pending::awaiting) {
            self.retire_issue(window, cx);
            return true;
        }
        false
    }

    pub(super) fn release_issue(&mut self, window: AnyWindowHandle, cx: &mut App) {
        if let Some(pending) = self.pending_issue.take() {
            let (review, dialog) = pending.retire();
            let _gone = self.owner.update(cx, |owner, cx| {
                owner.cancel_issue_review(review, cx);
            });
            if let Some(dialog) = dialog {
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
        let Some((review, op)) =
            pending.consent(confirmed, &window_state(window, cx), &self.facts(cx))
        else {
            self.pending_issue = Some(pending);
            self.retire_issue(window, cx);
            return false;
        };
        self.owner
            .update(cx, |owner, cx| owner.confirm_issue_review(review, op, cx))
            .unwrap_or(false)
    }

    fn issue_action(&mut self, op: IssueOp, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !machine::action_allowed(&op, &window_state(window, cx), self.model.read(cx)) {
            return;
        }
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
        let Some(opening) = machine::Opening::new(
            self.pending_issue.as_ref(),
            draft,
            &window_state(window, cx),
            &self.facts(cx),
        ) else {
            return false;
        };
        let view = cx.weak_entity();
        let review = self
            .owner
            .update(cx, |owner, cx| {
                owner.begin_issue_review(
                    opening.rearm(),
                    view,
                    window.window_handle(),
                    opening.live(),
                    cx,
                )
            })
            .ok()
            .flatten();
        let Some(review) = review else {
            return false;
        };
        self.pending_issue = Some(opening.pending(review));
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
        if !pending.awaits(request) {
            self.pending_issue = Some(pending);
            return false;
        }
        if pending.refuses(&card, &window_state(window, cx), &self.facts(cx)) {
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
            pending.offer(model, version, approval, dialog);
        });
        self.pending_issue = Some(pending);
        true
    }
}
