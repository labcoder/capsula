//! Session-fenced GitHub snapshots and explicit one-shot intents, without transport or GPUI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::strings::Text;
use kumochi_protocol::message::{Cleanup, GitHubOp, GitHubView, UserCode, UserError};

#[derive(Default)]
pub(crate) struct GitHubConnections {
    pub(crate) generation: Option<u64>,
    pub(crate) view: Option<GitHubView>,
    pub(crate) error: Option<UserError>,
    pub(crate) send_failed: bool,
    pub(crate) origin: Arc<()>,
    pub(crate) controls_origin: Arc<()>,
    waiting: Option<GitHubOp>,
    unexplained: bool,
    challenge: Option<UserCode>,
    challenge_until: Option<Instant>,
}

impl GitHubConnections {
    pub(crate) fn status(&self) -> Text {
        match self.waiting {
            Some(GitHubOp::Connect) => return Text::GitHubRequesting,
            Some(GitHubOp::CancelConnect) => return Text::GitHubCanceling,
            Some(GitHubOp::RefreshRepos) => return Text::GitHubRefreshing,
            Some(GitHubOp::SetDefaultRepo(_)) => return Text::GitHubSavingDefault,
            _ => {}
        }
        match &self.view {
            None => Text::GitHubWaiting,
            Some(GitHubView::SignedOut { cleanup: None }) => Text::GitHubSignedOut,
            Some(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running),
            }) => Text::GitHubRemoving,
            Some(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Refused),
            }) => Text::GitHubRemovalFailed,
            Some(GitHubView::SignedOut {
                cleanup: Some(Cleanup::NotDurable),
            }) => Text::GitHubNotDurable,
            Some(GitHubView::Requesting) => Text::GitHubRequesting,
            Some(GitHubView::AwaitingUser { .. }) => Text::GitHubAwaiting,
            Some(GitHubView::Connected { .. }) => Text::GitHubConnected,
            Some(GitHubView::NotInstalled { .. }) => Text::GitHubNotInstalled,
            Some(GitHubView::NeedsSignIn { .. }) => Text::GitHubNeedsSignIn,
            Some(GitHubView::NotVerified { .. }) => Text::GitHubNotVerified,
        }
    }

    pub(crate) fn commands(&self) -> &'static [(Text, GitHubOp)] {
        match &self.view {
            None | Some(GitHubView::SignedOut { cleanup: None }) => {
                &[(Text::GitHubConnect, GitHubOp::Connect)]
            }
            Some(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running),
            }) => &[],
            Some(GitHubView::SignedOut { .. }) => &[(Text::Retry, GitHubOp::RetryCleanup)],
            Some(GitHubView::Requesting | GitHubView::AwaitingUser { .. }) => {
                &[(Text::AccountCancel, GitHubOp::CancelConnect)]
            }
            Some(GitHubView::Connected { .. }) => &[
                (Text::GitHubRefresh, GitHubOp::RefreshRepos),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
            Some(GitHubView::NotInstalled { .. }) => &[(Text::GitHubSignOut, GitHubOp::SignOut)],
            Some(GitHubView::NotVerified { .. }) => &[
                (Text::GitHubCheckAgain, GitHubOp::RefreshRepos),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
            Some(GitHubView::NeedsSignIn { .. }) => &[
                (Text::GitHubSignInAgain, GitHubOp::Connect),
                (Text::GitHubSignOut, GitHubOp::SignOut),
            ],
        }
    }

    pub(crate) fn error_text(&self) -> Option<Text> {
        use kumochi_protocol::message::ErrorKey;
        self.error.as_ref().map(|error| match error.key() {
            ErrorKey::GitHubUnavailable => Text::GitHubUnavailable,
            ErrorKey::SignInTimedOut => Text::GitHubTimeout,
            ErrorKey::AccountOffline => Text::GitHubOffline,
            ErrorKey::CredentialsUnavailable | ErrorKey::SignOutNotDurable => {
                Text::GitHubCredentials
            }
            ErrorKey::RequestRefused => Text::GitHubRefused,
            ErrorKey::SettingsRefused => Text::GitHubDefaultRetained,
            ErrorKey::AccountMisconfigured => Text::GitHubConfiguration,
            _ => Text::GitHubFailure,
        })
    }

    pub(crate) fn login(&self) -> Option<&kumochi_protocol::message::GitHubLogin> {
        match &self.view {
            Some(
                GitHubView::Connected { login, .. }
                | GitHubView::NotInstalled { login }
                | GitHubView::NeedsSignIn { login }
                | GitHubView::NotVerified { login },
            ) => Some(login),
            None
            | Some(
                GitHubView::SignedOut { .. }
                | GitHubView::Requesting
                | GitHubView::AwaitingUser { .. },
            ) => None,
        }
    }

    pub(crate) fn disconnected(&mut self) {
        *self = Self::default();
    }

    fn accept(&mut self, current: Option<u64>, generation: u64) -> bool {
        if current != Some(generation) {
            return false;
        }
        if self.generation != Some(generation) {
            self.disconnected();
            self.generation = Some(generation);
        }
        true
    }

    pub(crate) fn receive(
        &mut self,
        current: Option<u64>,
        generation: u64,
        view: GitHubView,
        now: Instant,
    ) -> bool {
        if !self.accept(current, generation) {
            return false;
        }
        let settled = match &self.waiting {
            Some(GitHubOp::Connect) => matches!(
                view,
                GitHubView::Requesting
                    | GitHubView::AwaitingUser { .. }
                    | GitHubView::Connected { .. }
            ),
            Some(GitHubOp::SetDefaultRepo(repo)) => !matches!(
                &view,
                GitHubView::Connected { default, .. } if default.as_ref() != Some(repo)
            ),
            Some(GitHubOp::CancelConnect) => !matches!(
                view,
                GitHubView::Requesting | GitHubView::AwaitingUser { .. }
            ),
            _ => true,
        };
        if self.view.as_ref() != Some(&view) {
            self.controls_origin = Arc::default();
        }
        if self
            .view
            .as_ref()
            .is_some_and(|previous| match (previous, &view) {
                (
                    GitHubView::Connected {
                        login,
                        repos,
                        default,
                        ..
                    },
                    next,
                ) => match next {
                    GitHubView::Connected {
                        login: next_login,
                        repos: next_repos,
                        default: next_default,
                        ..
                    } => (login, repos, default) != (next_login, next_repos, next_default),
                    GitHubView::SignedOut { .. }
                    | GitHubView::Requesting
                    | GitHubView::AwaitingUser { .. }
                    | GitHubView::NotInstalled { .. }
                    | GitHubView::NeedsSignIn { .. }
                    | GitHubView::NotVerified { .. } => true,
                },
                (
                    GitHubView::SignedOut { .. }
                    | GitHubView::Requesting
                    | GitHubView::AwaitingUser { .. }
                    | GitHubView::NotInstalled { .. }
                    | GitHubView::NeedsSignIn { .. }
                    | GitHubView::NotVerified { .. },
                    next,
                ) => previous != next,
            })
        {
            self.origin = Arc::default();
        }
        if !self.unexplained && self.view.as_ref() != Some(&view) {
            self.error = None;
        }
        self.unexplained = false;
        if let GitHubView::AwaitingUser {
            user_code,
            expires_in_s,
        } = &view
        {
            let deadline = now + Duration::from_secs(u64::from(*expires_in_s));
            self.challenge_until = Some(
                self.challenge_until
                    .map_or(deadline, |until| until.min(deadline)),
            );
            self.challenge =
                (self.waiting != Some(GitHubOp::CancelConnect)).then(|| user_code.clone());
        } else {
            self.challenge = None;
        }
        self.view = Some(view);
        if settled {
            self.waiting = None;
        }
        self.send_failed = false;
        true
    }

    pub(crate) fn receive_error(
        &mut self,
        current: Option<u64>,
        generation: u64,
        error: UserError,
    ) -> bool {
        if !self.accept(current, generation) {
            return false;
        }
        self.error = Some(error);
        self.controls_origin = Arc::default();
        self.challenge = None;
        self.unexplained = true;
        self.waiting = None;
        true
    }

    pub(crate) fn allows(&self, current: Option<u64>, op: &GitHubOp) -> bool {
        if current.is_none() || current != self.generation {
            return false;
        }
        if self.waiting.is_some() && !matches!(op, GitHubOp::CancelConnect) {
            return false;
        }
        if self.waiting == Some(GitHubOp::CancelConnect) {
            return false;
        }
        if self.waiting == Some(GitHubOp::Connect) && *op == GitHubOp::CancelConnect {
            return true;
        }
        match &self.view {
            None => false,
            Some(GitHubView::SignedOut { cleanup }) => match cleanup {
                None => matches!(op, GitHubOp::Connect),
                Some(Cleanup::Refused | Cleanup::NotDurable) => {
                    matches!(op, GitHubOp::RetryCleanup)
                }
                Some(Cleanup::Running) => false,
            },
            Some(GitHubView::Requesting | GitHubView::AwaitingUser { .. }) => {
                matches!(op, GitHubOp::CancelConnect)
            }
            Some(GitHubView::Connected {
                repos,
                default,
                truncated,
                ..
            }) => match op {
                GitHubOp::RefreshRepos | GitHubOp::SignOut => true,
                GitHubOp::SetDefaultRepo(repo) => {
                    !truncated && repos.contains(repo) && default.as_ref() != Some(repo)
                }
                GitHubOp::Connect | GitHubOp::CancelConnect | GitHubOp::RetryCleanup => false,
            },
            Some(GitHubView::NotInstalled { .. }) => matches!(op, GitHubOp::SignOut),
            Some(GitHubView::NeedsSignIn { .. }) => {
                matches!(op, GitHubOp::Connect | GitHubOp::SignOut)
            }
            Some(GitHubView::NotVerified { .. }) => {
                matches!(op, GitHubOp::RefreshRepos | GitHubOp::SignOut)
            }
        }
    }

    pub(crate) fn send(
        &mut self,
        current: Option<u64>,
        op: GitHubOp,
        send: impl FnOnce(GitHubOp) -> bool,
    ) -> bool {
        if !self.allows(current, &op) {
            return false;
        }
        if !send(op.clone()) {
            self.send_failed = true;
            return false;
        }
        if op == GitHubOp::Connect {
            self.challenge = None;
            self.challenge_until = None;
        } else if op == GitHubOp::CancelConnect {
            self.challenge = None;
        }
        self.waiting = Some(op);
        self.error = None;
        self.unexplained = false;
        self.send_failed = false;
        true
    }

    pub(crate) fn code(&self, current: Option<u64>, now: Instant) -> Option<(&UserCode, u64)> {
        if current.is_none() || current != self.generation || self.error.is_some() {
            return None;
        }
        let GitHubView::AwaitingUser { user_code, .. } = self.view.as_ref()? else {
            return None;
        };
        let code = self.challenge.as_ref()?;
        let until = self.challenge_until?;
        let remaining = until.saturating_duration_since(now);
        (code == user_code && !remaining.is_zero()).then_some((
            code,
            remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0),
        ))
    }

    pub(crate) fn is_current(
        &self,
        current: Option<u64>,
        generation: Option<u64>,
        origin: &Arc<()>,
    ) -> bool {
        current.is_some()
            && current == generation
            && current == self.generation
            && Arc::ptr_eq(origin, &self.controls_origin)
    }
}

#[cfg(test)]
mod tests;
