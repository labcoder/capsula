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
    waiting: Option<GitHubOp>,
    unexplained: bool,
    challenge: Option<(UserCode, Instant)>,
}

impl GitHubConnections {
    pub(crate) fn status(&self) -> Text {
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
            _ => Text::GitHubFailure,
        })
    }

    pub(crate) fn login(&self) -> Option<&kumochi_protocol::message::GitHubLogin> {
        match &self.view {
            Some(
                GitHubView::Connected { login, .. }
                | GitHubView::NotInstalled { login }
                | GitHubView::NeedsSignIn { login },
            ) => Some(login),
            _ => None,
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
                    GitHubView::Connected {
                        login: next_login,
                        repos: next_repos,
                        default: next_default,
                        ..
                    },
                ) => (login, repos, default) != (next_login, next_repos, next_default),
                _ => previous != &view,
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
            match &mut self.challenge {
                Some((code, until)) if code == user_code => *until = (*until).min(deadline),
                _ => self.challenge = Some((user_code.clone(), deadline)),
            }
        }
        self.view = Some(view);
        self.waiting = None;
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
        match (&self.view, op) {
            (
                Some(GitHubView::SignedOut { cleanup: None } | GitHubView::NeedsSignIn { .. }),
                GitHubOp::Connect,
            )
            | (
                Some(GitHubView::Requesting | GitHubView::AwaitingUser { .. }),
                GitHubOp::CancelConnect,
            )
            | (
                Some(GitHubView::SignedOut {
                    cleanup: Some(Cleanup::Refused | Cleanup::NotDurable),
                }),
                GitHubOp::RetryCleanup,
            )
            | (
                Some(
                    GitHubView::Connected { .. }
                    | GitHubView::NotInstalled { .. }
                    | GitHubView::NeedsSignIn { .. },
                ),
                GitHubOp::SignOut,
            )
            | (Some(GitHubView::Connected { .. }), GitHubOp::RefreshRepos) => true,
            (
                Some(GitHubView::Connected { repos, default, .. }),
                GitHubOp::SetDefaultRepo(repo),
            ) => repos.contains(repo) && default.as_ref() != Some(repo),
            _ => false,
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
        }
        self.waiting = Some(op);
        self.error = None;
        self.unexplained = false;
        self.send_failed = false;
        true
    }

    pub(crate) fn code(&self, current: Option<u64>, now: Instant) -> Option<(&UserCode, u64)> {
        if current.is_none() || current != self.generation {
            return None;
        }
        let GitHubView::AwaitingUser { user_code, .. } = self.view.as_ref()? else {
            return None;
        };
        let (code, until) = self.challenge.as_ref()?;
        let remaining = until.saturating_duration_since(now);
        (code == user_code && !remaining.is_zero()).then_some((
            code,
            remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0),
        ))
    }
}

#[cfg(test)]
mod tests;
