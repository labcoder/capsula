//! The account step: each connection's section as Settings draws it, and the sign-ins this
//! window asked for. A dismissal (Finish later, Esc, the window's close) cancels one that hasn't
//! ended; Continue, Not now and the finish hand a waiting one over to the rest of the app.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div,
};
use kumochi_protocol::message::{
    AccountOp, AccountView, Action, ClaudeCodeView, ConnectionOp, ControlToBrain, ErrorKey,
    UserError,
};

use super::OnboardingView;
use crate::onboarding::account_complete;
use crate::presence::Presence;
use crate::presence::settings::accounts::account_section;
use crate::presence::settings::claude::claude_section;
use crate::settings::{Field, Settings};
use crate::sign_in_mark::{Seen, Shows};
use crate::strings::Strings;

/// A model connection is usable: the step's button reads Continue instead of Not now.
pub(super) fn usable(state: &Settings) -> bool {
    account_complete(state.accounts.view.as_ref())
        || matches!(state.claude.view, Some(ClaudeCodeView::Ready { .. }))
}

impl OnboardingView {
    /// A heading, then each connection's section without Sign out or Disconnect. Their buttons
    /// are this window's, so a sign-in they start is its own.
    pub(super) fn accounts(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let model = self.model.read(cx);
        let [heading, help] = strings.onboarding_accounts(model.value(Field::Assistant));
        let (chatgpt, claude) = (self.owner.clone(), self.owner.clone());
        div()
            .id("onboarding-accounts")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().child(heading))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(help),
                            ),
                    )
                    .child(account_section(
                        model,
                        move |op, cx| {
                            let _gone =
                                chatgpt.update(cx, |owner, cx| owner.onboarding_account_op(op, cx));
                        },
                        false,
                        None,
                        cx,
                    ))
                    .child(claude_section(
                        &self.model,
                        move |op, cx| {
                            let _gone =
                                claude.update(cx, |owner, cx| owner.onboarding_claude_op(op, cx));
                        },
                        None,
                        cx,
                    )),
            )
    }
}

impl Presence {
    /// A `ChatGPT` button in the account step. A sign-in it queued is this window's.
    pub(super) fn onboarding_account_op(&mut self, op: AccountOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        if self.queue_account_op(op, cx)
            && matches!(op, AccountOp::SignIn | AccountOp::EnablePlan)
            && let Some(generation) = generation
        {
            self.onboarding_chatgpt.sent(generation);
        }
    }

    /// A Claude Code button in the account step. A connect it queued is this window's.
    pub(super) fn onboarding_claude_op(&mut self, op: ConnectionOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        if self.queue_claude_op(op, cx)
            && op == ConnectionOp::ConnectClaudeCode
            && let Some(generation) = generation
        {
            self.onboarding_claude.sent(generation);
        }
    }

    /// An accepted `ChatGPT` view on connection `generation`; `previous` is the connection the
    /// view before it came on. A wait found on a connection's first view while the window shows
    /// is adopted: after a shell restart, nothing else could have started it.
    pub(in crate::presence) fn onboarding_account_seen(
        &mut self,
        generation: u64,
        previous: Option<u64>,
        cx: &mut Context<'_, Self>,
    ) {
        let view = self.settings.read(cx).accounts.view.as_ref();
        let complete = account_complete(view);
        let shows = if matches!(view, Some(AccountView::SigningIn)) {
            Shows::Waiting
        } else if complete {
            Shows::Ended
        } else {
            Shows::Other
        };
        if shows == Shows::Waiting && previous != Some(generation) && self.onboarding.is_some() {
            self.onboarding_chatgpt.adopt(generation);
        }
        match self.onboarding_chatgpt.view(generation, shows) {
            Seen::Nothing => {}
            Seen::Ended => {
                if complete && let Some(panel) = &self.onboarding {
                    panel.view.update(cx, |view, cx| {
                        view.plan = true;
                        cx.notify();
                    });
                }
            }
            Seen::SendCancel => {
                if self.send_chatgpt_cancel(cx) {
                    self.onboarding_chatgpt.cancel_sent();
                }
            }
        }
    }

    /// An accepted Claude Code view on connection `generation`, as for `ChatGPT`. The brain
    /// starts the login inside Connect and answers `SigningIn`, so any other answer ends it.
    pub(in crate::presence) fn onboarding_claude_seen(
        &mut self,
        generation: u64,
        previous: Option<u64>,
        cx: &mut Context<'_, Self>,
    ) {
        let shows = if self.settings.read(cx).claude.view == Some(ClaudeCodeView::SigningIn) {
            Shows::Waiting
        } else {
            Shows::Ended
        };
        if shows == Shows::Waiting && previous != Some(generation) && self.onboarding.is_some() {
            self.onboarding_claude.adopt(generation);
        }
        if self.onboarding_claude.view(generation, shows) == Seen::SendCancel
            && self.send_claude_cancel(cx)
        {
            self.onboarding_claude.cancel_sent();
        }
    }

    /// The brain ended a `ChatGPT` sign-in with an error offering a new one. Errors that offer
    /// only Retry come from renewals and refused starts, so they end nothing here.
    pub(in crate::presence) fn onboarding_sign_in_failed(&mut self, error: &UserError) {
        if matches!(
            error.key(),
            ErrorKey::BrowserUnavailable
                | ErrorKey::SignInTimedOut
                | ErrorKey::SignInFailed
                | ErrorKey::SignInOtherAccount
                | ErrorKey::AccountOffline
                | ErrorKey::CredentialsUnavailable
        ) && error.actions().contains(&Action::SignIn)
        {
            self.onboarding_chatgpt.failed();
        }
    }

    /// The window was dismissed: each sign-in it asked for that hasn't ended is cancelled once,
    /// past the model's gate, since the brain may not have answered or shown it yet. One that
    /// can't be sent stays owed; the window closes either way.
    pub(super) fn cancel_onboarding_sign_ins(&mut self, cx: &mut Context<'_, Self>) {
        if self.onboarding_chatgpt.take() && self.send_chatgpt_cancel(cx) {
            self.onboarding_chatgpt.cancel_sent();
        }
        if self.onboarding_claude.take() && self.send_claude_cancel(cx) {
            self.onboarding_claude.cancel_sent();
        }
    }

    /// The window opened over a waiting sign-in: after a shell restart, it's this window's.
    pub(super) fn adopt_onboarding_sign_ins(&mut self, cx: &Context<'_, Self>) {
        let state = self.settings.read(cx);
        if let (Some(AccountView::SigningIn), Some(generation)) =
            (&state.accounts.view, state.accounts.generation)
        {
            self.onboarding_chatgpt.adopt(generation);
        }
        if let (Some(ClaudeCodeView::SigningIn), Some(generation)) =
            (state.claude.view, state.claude.generation)
        {
            self.onboarding_claude.adopt(generation);
        }
    }

    fn send_chatgpt_cancel(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let (link, session) = (&self.link, &self.session);
        let sent = self.settings.update(cx, |state, cx| {
            cx.notify();
            state.accounts.cancel_sign_in(|op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(session, ControlToBrain::AccountOp(op))
                })
            })
        });
        if !sent {
            tracing::info!("onboarding's ChatGPT cancel is owed");
        }
        sent
    }

    fn send_claude_cancel(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let (link, session) = (&self.link, &self.session);
        let sent = self.settings.update(cx, |state, cx| {
            cx.notify();
            state.claude.cancel_sign_in(|op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(session, ControlToBrain::Connection(op))
                })
            })
        });
        if !sent {
            tracing::info!("onboarding's Claude Code cancel is owed");
        }
        sent
    }
}
