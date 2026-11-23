//! Authoritative account snapshots and one-shot intents, independent of GPUI and transport.

use crate::link::LinkEvent;
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, Cleanup, ErrorKey, PlanState, UserError,
};

#[derive(Default)]
pub struct Accounts {
    pub generation: Option<u64>,
    pub view: Option<AccountView>,
    pub error: Option<UserError>,
    pub send_failed: bool,
    pub waiting: Option<AccountOp>,
    pub unsupported: bool,
    unexplained: bool,
}

impl Accounts {
    pub fn receive(&mut self, current: Option<u64>, generation: u64, view: AccountView) -> bool {
        self.receive_snapshot(current, LinkEvent::Account { generation, view })
    }

    pub fn receive_snapshot(&mut self, current: Option<u64>, snapshot: LinkEvent) -> bool {
        let (generation, view, unsupported) = match snapshot {
            LinkEvent::Account { generation, view } => (generation, view, false),
            LinkEvent::AccountUnsupported { generation, view } => (generation, view, true),
            _ => return false,
        };
        if current != Some(generation) {
            return false;
        }
        if (unsupported && !self.unsupported)
            || (!self.unexplained && self.view.as_ref() != Some(&view))
        {
            self.error = None;
        }
        self.unexplained = false;
        self.view = Some(view);
        self.generation = Some(generation);
        self.unsupported = unsupported;
        self.waiting = None;
        self.send_failed = false;
        true
    }

    pub fn disconnected(&mut self) {
        *self = Self::default();
    }

    pub fn error(&mut self, error: &UserError) {
        if matches!(
            error.key(),
            ErrorKey::AccountRefused | ErrorKey::SignInNeeded
        ) && (!error.actions().contains(&Action::SignIn)
            || self
                .error
                .as_ref()
                .is_some_and(|error| error.key() == ErrorKey::RevocationBacklog))
        {
            return;
        }
        match error.key() {
            ErrorKey::SettingsRefused
            | ErrorKey::ConfigUnreadable
            | ErrorKey::UsageLimit
            | ErrorKey::RequestRefused
            | ErrorKey::ResponseUnreadable
            | ErrorKey::ModelUnavailable
            // shortcut: the Claude connections' errors have no Accounts row until C7a shows them.
            | ErrorKey::AnthropicKeyRejected
            | ErrorKey::AnthropicKeyFormat
            | ErrorKey::AnthropicUnreachable
            | ErrorKey::AnthropicRateLimited
            | ErrorKey::AnthropicCredit
            | ErrorKey::AnthropicAnswerLimit
            | ErrorKey::ClaudeCodeNotFound
            | ErrorKey::ClaudeCodeUnsupported
            | ErrorKey::ClaudeCodeSignedOut
            | ErrorKey::ClaudeCodeAuditFailed
            | ErrorKey::ClaudeCodePolicy
            | ErrorKey::ClaudeCodeInputUnsupported
            | ErrorKey::ClaudeCodeBudget
            | ErrorKey::ClaudeCodeCleanupPending
            | ErrorKey::ClaudeCodeUnavailable
            | ErrorKey::ClaudeCodeBusy
            // shortcut: voice errors have no Settings row until the voice settings show them.
            | ErrorKey::VoiceNoSpace
            | ErrorKey::VoiceDownload
            | ErrorKey::VoiceCorrupt
            | ErrorKey::VoiceStorage
            | ErrorKey::VoiceLoad
            | ErrorKey::VoiceUnavailable
            | ErrorKey::VoiceModelPath
            | ErrorKey::HoldToTalkOff
            | ErrorKey::GitHubUnavailable
            | ErrorKey::UnsupportedPlatform
            | ErrorKey::DrawingReplacementFailed
            | ErrorKey::DrawingNotReturned
            | ErrorKey::ScreenUnavailable
            | ErrorKey::ScreenPictureStopped
            | ErrorKey::ScreenConnectionChanged
            | ErrorKey::ConnectionNeeded => {}
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
            | ErrorKey::AccountRefused
            | ErrorKey::SignInNeeded
            | ErrorKey::RenewalUnconfirmed
            | ErrorKey::PlanUnavailable => {
                self.error = Some(error.clone());
                self.unexplained = true;
            }
        }
    }

    pub fn allows(&self, op: AccountOp) -> bool {
        if self.waiting.is_some() {
            return false;
        }
        if self.unsupported
            && (matches!(op, AccountOp::SignIn | AccountOp::EnablePlan)
                || op == AccountOp::Retry
                    && !matches!(self.view, Some(AccountView::SigningOut { .. })))
        {
            return false;
        }
        if op == AccountOp::SignIn
            && self
                .error
                .as_ref()
                .is_some_and(|error| error.key() == ErrorKey::RevocationBacklog)
        {
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
                AccountOp::SignIn => self.sign_in_again(),
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

    pub fn sign_in_again(&self) -> bool {
        if self.unsupported {
            return false;
        }
        matches!(
            self.view,
            Some(AccountView::SignedIn {
                recovery: Some(AccountRecovery::SignInAgain),
                ..
            })
        ) || matches!(self.view, Some(AccountView::SignedIn { .. }))
            && self.error_action(Action::SignIn)
            && self.error.as_ref().is_some_and(|error| {
                matches!(
                    error.key(),
                    ErrorKey::AccountRefused
                        | ErrorKey::SignInNeeded
                        | ErrorKey::SessionEnded
                        | ErrorKey::RenewalUnconfirmed
                )
            })
    }

    fn error_action(&self, action: Action) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.actions().contains(&action))
    }

    pub fn send(&mut self, op: AccountOp, send: impl FnOnce(AccountOp) -> bool) -> bool {
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

    /// Sends `CancelSignIn` without asking `allows`, for a dismissal that must cancel a sign-in
    /// the brain hasn't answered or shown yet. The brain ignores it when nothing is signing in.
    pub fn cancel_sign_in(&mut self, send: impl FnOnce(AccountOp) -> bool) -> bool {
        if !send(AccountOp::CancelSignIn) {
            return false;
        }
        self.waiting = Some(AccountOp::CancelSignIn);
        true
    }
}

#[cfg(test)]
mod tests;
