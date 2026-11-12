//! The sign-ins a surface asked for, until each ends: one [`SignInMark`] per surface and
//! connection, for the onboarding window and for Settings. A surface's dismissal cancels its own
//! sign-in that hasn't ended. A sign-in started from chat belongs to neither, and only onboarding
//! adopts a waiting one after a shell restart.

use gpui_kit::Context;
use kumochi_protocol::message::{
    AccountOp, AccountView, Action, ClaudeCodeView, ConnectionOp, ControlToBrain, ErrorKey,
    UserError,
};

use super::Presence;
use crate::onboarding::account_complete;
use crate::sign_in_mark::{Seen, Shows};

impl Presence {
    /// A `ChatGPT` button in Settings. A sign-in it queued is Settings' own.
    pub(super) fn settings_account_op(&mut self, op: AccountOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        if self.account_op(op, cx)
            && matches!(op, AccountOp::SignIn | AccountOp::EnablePlan)
            && let Some(generation) = generation
        {
            self.settings_chatgpt.sent(generation);
        }
    }

    /// A Claude Code button in Settings. A connect it queued is Settings' own.
    pub(super) fn settings_claude_op(&mut self, op: ConnectionOp, cx: &mut Context<'_, Self>) {
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        if self.claude_op(op, cx)
            && op == ConnectionOp::ConnectClaudeCode
            && let Some(generation) = generation
        {
            self.settings_claude.sent(generation);
        }
    }

    /// Settings closed, however it closed: each sign-in it asked for that hasn't ended is
    /// cancelled once, past the model's gate. One that can't be sent stays owed.
    pub(super) fn cancel_settings_sign_ins(&mut self, cx: &mut Context<'_, Self>) {
        if self.settings_chatgpt.take() && self.send_chatgpt_cancel(cx) {
            self.settings_chatgpt.cancel_sent();
        }
        if self.settings_claude.take() && self.send_claude_cancel(cx) {
            self.settings_claude.cancel_sent();
        }
    }

    /// An accepted `ChatGPT` view on connection `generation`; `previous` is the connection the
    /// view before it came on. A wait found on a connection's first view while the onboarding
    /// window shows is adopted: after a shell restart, nothing else could have started it.
    pub(super) fn account_seen(
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
                if complete {
                    self.onboarding_signed_in(cx);
                }
            }
            Seen::SendCancel => {
                if self.send_chatgpt_cancel(cx) {
                    self.onboarding_chatgpt.cancel_sent();
                }
            }
        }
        if self.settings_chatgpt.view(generation, shows) == Seen::SendCancel
            && self.send_chatgpt_cancel(cx)
        {
            self.settings_chatgpt.cancel_sent();
        }
    }

    /// An accepted Claude Code view on connection `generation`, as for `ChatGPT`. The brain
    /// starts the login inside Connect and answers `SigningIn`, so any other answer ends it.
    pub(super) fn claude_seen(
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
        if self.settings_claude.view(generation, shows) == Seen::SendCancel
            && self.send_claude_cancel(cx)
        {
            self.settings_claude.cancel_sent();
        }
    }

    /// The brain ended a `ChatGPT` sign-in with an error offering a new one. Errors that offer
    /// only Retry come from renewals and refused starts, so they end nothing here.
    pub(super) fn chatgpt_sign_in_failed(&mut self, error: &UserError) {
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
            self.settings_chatgpt.failed();
        }
    }

    pub(super) fn send_chatgpt_cancel(&mut self, cx: &mut Context<'_, Self>) -> bool {
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
            tracing::info!("a ChatGPT sign-in cancel is owed");
        }
        sent
    }

    pub(super) fn send_claude_cancel(&mut self, cx: &mut Context<'_, Self>) -> bool {
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
            tracing::info!("a Claude Code sign-in cancel is owed");
        }
        sent
    }
}
