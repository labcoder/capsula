//! Authoritative account snapshots and one-shot intents, independent of GPUI and transport.

use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, Cleanup, ErrorKey, PlanState, SettingsView,
    UserError,
};

use crate::strings::{Strings, Text};

#[derive(Debug, PartialEq, Eq)]
pub enum StatusLine<'a> {
    Note(Text),
    Local {
        heading: Text,
        error: Text,
        help: Text,
    },
    Remote {
        heading: Text,
        note: Text,
    },
    Backlog {
        lead: Text,
        followups: [Text; 3],
    },
    Error(&'a UserError),
    SendFailed(Text),
}

#[derive(Default)]
pub struct Accounts {
    pub generation: Option<u64>,
    pub view: Option<AccountView>,
    pub error: Option<UserError>,
    pub send_failed: bool,
    pub waiting: Option<AccountOp>,
    unexplained: bool,
}

impl Accounts {
    pub fn receive(&mut self, current: Option<u64>, generation: u64, view: AccountView) -> bool {
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
            | ErrorKey::UnsupportedPlatform
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

    /// The heading and complete copy for the account row, in display order.
    pub fn status(&self, settings: Option<&SettingsView>) -> (Text, Vec<StatusLine<'_>>) {
        use StatusLine::{Backlog, Local, Note, Remote, SendFailed};

        let local = |error, help| Local {
            heading: Text::AccountLocal,
            error,
            help,
        };
        let remote = |note| Remote {
            heading: Text::AccountRemote,
            note,
        };
        let mut lines = Vec::new();
        let status = match &self.view {
            None if settings.is_none() => Text::AccountOffline,
            None => Text::AccountWaiting,
            Some(AccountView::SignedOut {
                revocation_unconfirmed,
            }) => {
                if *revocation_unconfirmed {
                    lines.push(remote(Text::AccountRevocationUnconfirmed));
                } else {
                    lines.push(Note(Text::AccountUsePlan));
                }
                Text::AccountSignedOut
            }
            Some(AccountView::SigningIn) => {
                lines.push(Note(Text::AccountReplaceHelp));
                Text::AccountSigningIn
            }
            Some(AccountView::Busy) => {
                lines.push(Note(Text::AccountBusyHelp));
                Text::AccountBusy
            }
            Some(view @ AccountView::SigningOut { cleanup, .. }) => {
                lines.push(match cleanup {
                    Cleanup::Running => Note(Text::AccountStopped),
                    Cleanup::Refused => {
                        local(Text::AccountCleanupRefused, Text::AccountCleanupRefusedHelp)
                    }
                    Cleanup::NotDurable => {
                        local(Text::AccountNotDurable, Text::AccountNotDurableHelp)
                    }
                });
                lines.extend(cleanup_notice(view).map(remote));
                if *cleanup == Cleanup::Running {
                    Text::AccountSigningOut
                } else {
                    Text::AccountSignOutNeedsRetry
                }
            }
            Some(AccountView::SignedIn { plan, recovery, .. }) => match recovery {
                Some(AccountRecovery::SignInAgain) => Text::AccountSessionEnded,
                Some(AccountRecovery::Retry) => {
                    if self.error.is_none() {
                        lines.push(Note(Text::AccountRetryHelp));
                    }
                    Text::AccountAttention
                }
                None => match plan {
                    PlanState::Enabled => Text::AccountUsingPlan,
                    PlanState::Declined => Text::AccountPlanOff,
                    PlanState::Unavailable => Text::AccountUnavailable,
                },
            },
        };
        lines.extend(self.status_error().map(|error| {
            if error.key() == ErrorKey::RevocationBacklog {
                Backlog {
                    lead: Text::AccountBacklogLead,
                    followups: [
                        Text::AccountBacklogRetry,
                        Text::AccountBacklogExpiry,
                        Text::AccountBacklogRecovery,
                    ],
                }
            } else {
                StatusLine::Error(error)
            }
        }));
        lines.extend(
            self.send_failed
                .then_some(SendFailed(Text::AccountSendFailed)),
        );
        (status, lines)
    }

    pub fn status_identity(&self, strings: &Strings) -> Option<String> {
        let Some(AccountView::SignedIn { email, .. }) = &self.view else {
            return None;
        };
        Some(email.as_ref().map_or_else(
            || strings.get(Text::AccountIdentity),
            |email| email.as_str().to_owned(),
        ))
    }

    /// The error to show in the account row, if the status doesn't already say it.
    fn status_error(&self) -> Option<&UserError> {
        self.error.as_ref().filter(|error| {
            !matches!(
                (error.key(), &self.view),
                (
                    ErrorKey::SignOutNotDurable,
                    Some(AccountView::SigningOut {
                        cleanup: Cleanup::NotDurable,
                        ..
                    })
                ) | (
                    ErrorKey::SessionEnded,
                    Some(AccountView::SignedIn {
                        recovery: Some(AccountRecovery::SignInAgain),
                        ..
                    })
                ) | (
                    ErrorKey::PlanUnavailable,
                    Some(AccountView::SignedIn {
                        plan: PlanState::Unavailable,
                        recovery: None,
                        ..
                    })
                )
            )
        })
    }

    pub fn allows(&self, op: AccountOp) -> bool {
        if self.waiting.is_some() {
            return false;
        }
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.key() == ErrorKey::UnsupportedPlatform)
            && !matches!(self.view, Some(AccountView::SigningOut { .. }))
            && matches!(op, AccountOp::SignIn | AccountOp::Retry)
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

fn cleanup_notice(view: &AccountView) -> Option<Text> {
    matches!(
        view,
        AccountView::SigningOut {
            cleanup: Cleanup::Refused | Cleanup::NotDurable,
            revocation_unconfirmed: true,
        }
    )
    .then_some(Text::AccountRevocationPending)
}

#[cfg(test)]
mod tests;
