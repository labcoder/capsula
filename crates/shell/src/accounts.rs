//! Authoritative account snapshots and one-shot intents, independent of GPUI and transport.

use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, Cleanup, ErrorKey, PlanState, UserError,
};

#[derive(Default)]
pub(crate) struct Accounts {
    pub(crate) generation: Option<u64>,
    pub(crate) view: Option<AccountView>,
    pub(crate) error: Option<UserError>,
    pub(crate) send_failed: bool,
    pub(crate) waiting: Option<AccountOp>,
    unexplained: bool,
}

impl Accounts {
    pub(crate) fn receive(
        &mut self,
        current: Option<u64>,
        generation: u64,
        view: AccountView,
    ) -> bool {
        if current != Some(generation) {
            return false;
        }
        if !self.unexplained && self.view.as_ref() != Some(&view) {
            self.error = None;
        }
        self.unexplained = false;
        self.view = Some(view);
        self.generation = Some(generation);
        self.waiting = None;
        self.send_failed = false;
        true
    }

    pub(crate) fn disconnected(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn error(&mut self, error: &UserError) {
        match error.key() {
            ErrorKey::SettingsRefused
            | ErrorKey::ConfigUnreadable
            | ErrorKey::UsageLimit
            | ErrorKey::AccountRefused
            | ErrorKey::RequestRefused
            | ErrorKey::ResponseUnreadable
            | ErrorKey::SignInNeeded
            | ErrorKey::ModelUnavailable
            // shortcut: the Claude connections' errors have no Accounts row until C7a shows them.
            | ErrorKey::AnthropicKeyRejected
            | ErrorKey::AnthropicKeyFormat
            | ErrorKey::AnthropicUnreachable
            | ErrorKey::AnthropicRateLimited
            | ErrorKey::AnthropicCredit
            | ErrorKey::ClaudeCodeNotFound
            | ErrorKey::ClaudeCodeUnsupported
            | ErrorKey::ClaudeCodeSignedOut
            | ErrorKey::ClaudeCodeAuditFailed
            | ErrorKey::ClaudeCodePolicy
            | ErrorKey::ClaudeCodeInputUnsupported
            | ErrorKey::ClaudeCodeBudget
            | ErrorKey::ClaudeCodeCleanupPending => {}
            ErrorKey::SignInTimedOut
            | ErrorKey::SignInFailed
            | ErrorKey::SignInOtherAccount
            | ErrorKey::BrowserUnavailable
            | ErrorKey::AccountOffline
            | ErrorKey::CredentialsUnavailable
            | ErrorKey::RevocationBacklog
            | ErrorKey::TrustStoreUnavailable
            | ErrorKey::PlanNotEnabled
            | ErrorKey::AccountMisconfigured
            | ErrorKey::SignOutNotDurable
            | ErrorKey::SessionEnded
            | ErrorKey::PlanUnavailable => {
                self.error = Some(error.clone());
                self.unexplained = true;
            }
        }
    }

    pub(crate) fn allows(&self, op: AccountOp) -> bool {
        if self.waiting.is_some() {
            return false;
        }
        match (&self.view, op) {
            (Some(AccountView::SignedOut { .. } | AccountView::SigningIn), AccountOp::SignIn)
            | (Some(AccountView::SigningIn), AccountOp::CancelSignIn)
            | (Some(AccountView::SignedIn { .. }), AccountOp::SignOut)
            | (
                Some(
                    AccountView::Busy
                    | AccountView::SigningOut {
                        cleanup: Cleanup::Refused | Cleanup::NotDurable,
                        ..
                    },
                ),
                AccountOp::Retry,
            ) => true,
            (Some(AccountView::SignedIn { plan, recovery, .. }), op) => match op {
                AccountOp::SignIn => *recovery == Some(AccountRecovery::SignInAgain),
                AccountOp::EnablePlan => recovery.is_none() && *plan == PlanState::Declined,
                AccountOp::Retry => {
                    *recovery == Some(AccountRecovery::Retry) || self.error_action(Action::Retry)
                }
                _ => false,
            },
            (Some(AccountView::SignedOut { .. }), AccountOp::Retry) => {
                self.error_action(Action::Retry)
            }
            _ => false,
        }
    }

    fn error_action(&self, action: Action) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.actions().contains(&action))
    }

    pub(crate) fn send(&mut self, op: AccountOp, send: impl FnOnce(AccountOp) -> bool) -> bool {
        if !self.allows(op) {
            return false;
        }
        if !send(op) {
            self.send_failed = true;
            return false;
        }
        self.error = None;
        self.unexplained = false;
        self.send_failed = false;
        self.waiting = Some(op);
        true
    }
}

#[cfg(test)]
mod tests;
