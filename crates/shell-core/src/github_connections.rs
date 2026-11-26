//! Session-fenced GitHub snapshots and explicit one-shot intents, without transport or GPUI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::strings::Text;
use kumochi_protocol::message::{Cleanup, ErrorKey, GitHubOp, GitHubView, UserCode, UserError};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum AccountsVisit {
    #[default]
    BeforeFirstConnection,
    OpenedBeforeFirstConnection,
    Closed,
    Waiting,
    Requested,
}

#[derive(Default)]
pub struct GitHubConnections {
    pub generation: Option<u64>,
    pub view: Option<GitHubView>,
    pub error: Option<UserError>,
    pub send_failed: bool,
    pub origin: Arc<()>,
    pub controls_origin: Arc<()>,
    waiting: Option<GitHubOp>,
    accounts_visit: AccountsVisit,
    unexplained: bool,
    challenge: Option<UserCode>,
    challenge_until: Option<Instant>,
}

impl GitHubConnections {
    pub fn status(&self) -> Text {
        match self.waiting {
            Some(GitHubOp::Connect) => return Text::GitHubRequesting,
            Some(GitHubOp::CancelConnect) => return Text::GitHubCanceling,
            Some(GitHubOp::RefreshRepos | GitHubOp::ListReposIfUnlisted) => {
                return Text::GitHubRefreshing;
            }
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
            Some(GitHubView::ConnectedUnlisted { listing: true, .. }) => Text::GitHubRefreshing,
            Some(GitHubView::Connected { .. } | GitHubView::ConnectedUnlisted { .. }) => {
                Text::GitHubConnected
            }
            Some(GitHubView::NotInstalled { .. }) => Text::GitHubNotInstalled,
            Some(GitHubView::NeedsSignIn { .. }) => Text::GitHubNeedsSignIn,
            Some(GitHubView::NotVerified { .. }) => Text::GitHubNotVerified,
        }
    }

    pub fn commands(&self) -> &'static [(Text, GitHubOp)] {
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
            Some(GitHubView::Connected { .. } | GitHubView::ConnectedUnlisted { .. }) => &[
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

    pub fn error_text(&self) -> Option<Text> {
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

    pub fn login(&self) -> Option<&kumochi_protocol::message::GitHubLogin> {
        match &self.view {
            Some(
                GitHubView::Connected { login, .. }
                | GitHubView::ConnectedUnlisted { login, .. }
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

    pub fn connecting(&mut self) {
        if self.accounts_visit != AccountsVisit::OpenedBeforeFirstConnection {
            self.disconnected();
        }
    }

    pub fn connected(&mut self, generation: u64) {
        let opened_early = self.accounts_visit == AccountsVisit::OpenedBeforeFirstConnection;
        *self = Self {
            accounts_visit: AccountsVisit::Closed,
            ..Self::default()
        };
        if opened_early {
            self.generation = Some(generation);
            self.accounts_visit = AccountsVisit::Waiting;
        }
    }

    pub fn disconnected(&mut self) {
        self.close_accounts_visit();
        *self = Self {
            accounts_visit: self.accounts_visit,
            ..Self::default()
        };
    }

    pub fn close_accounts_visit(&mut self) {
        self.accounts_visit = match self.accounts_visit {
            AccountsVisit::BeforeFirstConnection | AccountsVisit::OpenedBeforeFirstConnection => {
                AccountsVisit::BeforeFirstConnection
            }
            _ => AccountsVisit::Closed,
        };
    }

    pub fn open_accounts_visit(&mut self, current: Option<u64>) {
        self.close_accounts_visit();
        if let Some(generation) = current {
            self.accept(current, generation);
            if matches!(
                self.view,
                None | Some(
                    GitHubView::Requesting
                        | GitHubView::AwaitingUser { .. }
                        | GitHubView::ConnectedUnlisted { listing: false, .. }
                )
            ) {
                self.accounts_visit = AccountsVisit::Waiting;
            }
        } else if self.accounts_visit == AccountsVisit::BeforeFirstConnection {
            self.accounts_visit = AccountsVisit::OpenedBeforeFirstConnection;
        }
    }

    pub fn list_on_accounts_visit(&self, current: Option<u64>) -> bool {
        self.accounts_visit == AccountsVisit::Waiting
            && self.allows(current, &GitHubOp::ListReposIfUnlisted)
    }

    fn accept(&mut self, current: Option<u64>, generation: u64) -> bool {
        if current != Some(generation) {
            return false;
        }
        if self.generation != Some(generation) {
            self.connected(generation);
            self.generation = Some(generation);
        }
        true
    }

    pub fn receive(
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
                    | GitHubView::ConnectedUnlisted { .. }
            ),
            Some(GitHubOp::SetDefaultRepo(repo)) => !matches!(
                &view,
                GitHubView::Connected { default, .. } if default.as_ref() != Some(repo)
            ),
            Some(GitHubOp::CancelConnect) => !matches!(
                view,
                GitHubView::Requesting | GitHubView::AwaitingUser { .. }
            ),
            Some(GitHubOp::SignOut) => matches!(view, GitHubView::SignedOut { .. }),
            _ => true,
        };
        self.accounts_visit = match (self.accounts_visit, &view) {
            (AccountsVisit::Waiting | AccountsVisit::Requested, GitHubView::Requesting)
            | (
                AccountsVisit::Waiting,
                GitHubView::AwaitingUser { .. }
                | GitHubView::ConnectedUnlisted { listing: false, .. },
            ) => AccountsVisit::Waiting,
            _ => AccountsVisit::Closed,
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
                    | GitHubView::ConnectedUnlisted { .. }
                    | GitHubView::NotInstalled { .. }
                    | GitHubView::NeedsSignIn { .. }
                    | GitHubView::NotVerified { .. } => true,
                },
                (
                    GitHubView::SignedOut { .. }
                    | GitHubView::Requesting
                    | GitHubView::AwaitingUser { .. }
                    | GitHubView::ConnectedUnlisted { .. }
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

    pub fn receive_error(
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
        self.accounts_visit = AccountsVisit::Closed;
        true
    }

    pub fn allows(&self, current: Option<u64>, op: &GitHubOp) -> bool {
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
                GitHubOp::Connect
                | GitHubOp::CancelConnect
                | GitHubOp::RetryCleanup
                | GitHubOp::ListReposIfUnlisted => false,
            },
            Some(GitHubView::ConnectedUnlisted { listing, .. }) => {
                *op == GitHubOp::SignOut
                    || (!listing
                        && matches!(op, GitHubOp::RefreshRepos | GitHubOp::ListReposIfUnlisted))
            }
            Some(GitHubView::NotInstalled { .. }) => matches!(op, GitHubOp::SignOut),
            Some(GitHubView::NeedsSignIn { .. }) => {
                matches!(op, GitHubOp::Connect | GitHubOp::SignOut)
            }
            Some(GitHubView::NotVerified { .. }) => {
                matches!(op, GitHubOp::RefreshRepos | GitHubOp::SignOut)
            }
        }
    }

    pub fn send(
        &mut self,
        current: Option<u64>,
        op: GitHubOp,
        send: impl FnOnce(GitHubOp) -> bool,
    ) -> bool {
        if !self.allows(current, &op) {
            return false;
        }
        if !send(op.clone()) {
            if op == GitHubOp::ListReposIfUnlisted {
                self.accounts_visit = AccountsVisit::Closed;
            }
            self.send_failed = true;
            return false;
        }
        match op {
            GitHubOp::ListReposIfUnlisted if self.accounts_visit == AccountsVisit::Waiting => {
                self.accounts_visit = AccountsVisit::Requested;
            }
            GitHubOp::SignOut | GitHubOp::CancelConnect => {
                self.accounts_visit = AccountsVisit::Closed;
            }
            _ => {}
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

    pub fn code(&self, current: Option<u64>, now: Instant) -> Option<(&UserCode, u64)> {
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

    pub fn is_current(
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
