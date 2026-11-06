//! GitHub device sign-in: inputs in, effects out; no requests or credential I/O.

use super::{
    DeviceCodeValue, GitHubToken,
    api::{ApiError, DeviceCode, GitHubUser, TokenError, TokenReply},
};
use kumochi_protocol::{
    Secret,
    message::{
        Cleanup, ErrorKey, GitHubLogin, GitHubOp, GitHubView, MAX_GITHUB_REPOS, RepoName, UserError,
    },
};
use kumochi_scene::text::Bounded;
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

/// Identifies the attempt and connection that admitted work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Single-use sign-in identity.
    pub attempt: u64,
    /// Monotonic local credential-use fence.
    pub generation: u64,
}

/// A checked account awaiting durable ownership.
pub struct Account {
    /// Only the runtime and credential lane may use this bearer.
    pub token: Secret<GitHubToken>,
    /// Identity verified by the authenticated user endpoint.
    pub user: GitHubUser,
}
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubAccount")
    }
}

/// Local cleanup result, never permission to resume token use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    /// Removal committed and no names remain.
    Clean,
    /// Removal committed but deletion was refused.
    Refused,
    /// Removal could not be committed; restart may restore the account.
    NotDurable,
}

/// A shell, clock, network or serialized credential-worker event.
pub enum Input {
    /// A user operation.
    Op(GitHubOp),
    /// Initial credential read; unreadable records are cleaned by the lane first.
    Loaded {
        /// Startup fence.
        stamp: Stamp,
        /// Account or cleanup outcome; a vault-open failure is reported as no account.
        result: Result<Option<Account>, Removed>,
    },
    /// One device-code request finished.
    Code {
        /// Request identity.
        stamp: Stamp,
        /// API-validated challenge or refusal.
        result: Result<DeviceCode, ApiError>,
    },
    /// One token poll finished.
    Poll {
        /// Poll identity.
        stamp: Stamp,
        /// API-validated token or refusal.
        result: Result<TokenReply, ApiError>,
    },
    /// The authenticated identity check finished.
    User {
        /// Check identity.
        stamp: Stamp,
        /// Checked identity or refusal.
        result: Result<GitHubUser, ApiError>,
    },
    /// A serialized save finished.
    Saved {
        /// Save identity.
        stamp: Stamp,
        /// Whether the account has a durable owner.
        saved: bool,
    },
    /// Explicit cleanup finished.
    Removed {
        /// Sign-out fence.
        generation: u64,
        /// Durable and physical cleanup outcome.
        result: Removed,
    },
    /// A repository listing finished.
    Repos {
        /// Listing fence.
        generation: u64,
        /// Names in GitHub's spelling, not user-supplied names.
        names: Bounded<RepoName, MAX_GITHUB_REPOS>,
        /// More repositories exist beyond this bounded list.
        truncated: bool,
    },
    /// A consumer received 401.
    Unauthorized {
        /// Consumer fence.
        generation: u64,
    },
    /// A scheduled poll or deadline is due.
    Tick,
}

/// Work for the runtime; every completion keeps the original fence.
pub enum Effect {
    /// Request a challenge using the configured public client ID.
    RequestCode {
        /// Completion identity.
        stamp: Stamp,
    },
    /// Send one poll, watching cancellation and the attempt deadline.
    Poll {
        /// Completion identity.
        stamp: Stamp,
        /// Secret device credential.
        code: Secret<DeviceCodeValue>,
    },
    /// Wake at this instant, or end earlier on cancellation/shutdown.
    Wake(Instant),
    /// Verify the approving account before saving.
    ReadUser {
        /// Completion identity.
        stamp: Stamp,
        /// Bearer used only for the checked user endpoint.
        token: Secret<GitHubToken>,
    },
    /// Queue a save on the credential lane, rechecking the fence before writing.
    Save {
        /// Completion identity.
        stamp: Stamp,
        /// Checked account to persist.
        account: Account,
    },
    /// Queue durable removal and explicit cleanup behind any outstanding save.
    Remove {
        /// Completion fence.
        generation: u64,
    },
    /// Cancel old requests and wakeups, but never discard queued credential cleanup.
    CancelAll,
    /// Fetch the installed repository intersection.
    RefreshRepos {
        /// Completion fence.
        generation: u64,
    },
    /// Persist a fetched repository spelling.
    SaveDefault(RepoName),
    /// Publish the authoritative view.
    View(GitHubView),
    /// Show a content-free catalog error.
    Error(UserError),
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubInput")
    }
}
impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubEffect")
    }
}

struct Attempt {
    challenge: DeviceCode,
    deadline: Instant,
    next_poll: Instant,
    polling: bool,
}
impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubAttempt")
    }
}
enum State {
    Loading,
    SignedOut(Option<Cleanup>),
    Requesting,
    Awaiting(Attempt),
    Verifying {
        token: Secret<GitHubToken>,
        loaded_user: Option<GitHubUser>,
    },
    Saving(Account),
    Connected(Account),
    NeedsSignIn(GitHubLogin),
}

/// Owns the one attempt, bearer and authoritative connection generation.
pub struct Machine {
    state: State,
    stamp: Stamp,
    fence: Arc<AtomicU64>,
    deadline: Option<Instant>,
    repos: Bounded<RepoName, MAX_GITHUB_REPOS>,
    truncated: bool,
    default: Option<RepoName>,
}
impl fmt::Debug for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubMachine")
    }
}

impl Machine {
    /// Starts fenced until Loaded arrives. Queue the startup load with `stamp()`.
    pub fn new(default: Option<RepoName>) -> Self {
        Self {
            state: State::Loading,
            stamp: Stamp {
                attempt: 0,
                generation: 0,
            },
            fence: Arc::new(AtomicU64::new(0)),
            deadline: None,
            repos: Bounded::default(),
            truncated: false,
            default,
        }
    }

    /// Identity to attach to newly dispatched work.
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    /// Consumers load this with Acquire immediately before using a bearer or saving.
    pub fn fence(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.fence)
    }

    /// A projection with no bearer, device credential or provider URL.
    pub fn view(&self, now: Instant) -> GitHubView {
        match &self.state {
            State::Loading => GitHubView::SignedOut { cleanup: None },
            State::SignedOut(cleanup) => GitHubView::SignedOut { cleanup: *cleanup },
            State::Requesting | State::Verifying { .. } | State::Saving(_) => {
                GitHubView::Requesting
            }
            State::Awaiting(a) => GitHubView::AwaitingUser {
                user_code: a.challenge.user_code.clone(),
                expires_in_s: u16::try_from(a.deadline.saturating_duration_since(now).as_secs())
                    .unwrap_or(900),
            },
            State::Connected(account) => GitHubView::Connected {
                login: account.user.login.clone(),
                repos: self.repos.clone(),
                truncated: self.truncated,
                default: self
                    .default
                    .as_ref()
                    .filter(|repo| self.repos.contains(repo))
                    .cloned(),
            },
            State::NeedsSignIn(login) => GitHubView::NeedsSignIn {
                login: login.clone(),
            },
        }
    }

    fn advance(&mut self) {
        self.stamp.attempt = self
            .stamp
            .attempt
            .checked_add(1)
            .expect("attempt counter exhausted");
        self.stamp.generation = self
            .stamp
            .generation
            .checked_add(1)
            .expect("generation counter exhausted");
        self.fence.store(self.stamp.generation, Ordering::Release);
        self.deadline = None;
        self.repos = Bounded::new(Vec::new()).expect("empty repository list is bounded");
        self.truncated = false;
    }

    fn fail(&mut self, key: ErrorKey, effects: &mut Vec<Effect>) {
        self.cancel(effects);
        effects.push(Effect::Error(key.into()));
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) {
        let saving = matches!(self.state, State::Saving(_));
        self.advance();
        self.state = State::SignedOut(saving.then_some(Cleanup::Running));
        effects.push(Effect::CancelAll);
        if saving {
            effects.push(Effect::Remove {
                generation: self.stamp.generation,
            });
        }
    }

    /// Apply one event at the caller's clock. Ended attempts cannot be revived by completions.
    ///
    /// # Panics
    /// If a process exhausts its 64-bit attempt or generation counter.
    #[expect(
        clippy::too_many_lines,
        reason = "one ordered transition table keeps state ownership together"
    )]
    pub fn on(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.fail(ErrorKey::SignInTimedOut, &mut effects);
        }
        match input {
            Input::Op(GitHubOp::SignOut) => {
                self.advance();
                self.state = State::SignedOut(Some(Cleanup::Running));
                effects.extend([
                    Effect::CancelAll,
                    Effect::Remove {
                        generation: self.stamp.generation,
                    },
                ]);
            }
            Input::Op(GitHubOp::CancelConnect)
                if self.deadline.is_some() || matches!(self.state, State::Requesting) =>
            {
                self.cancel(&mut effects);
            }
            Input::Op(GitHubOp::Connect)
                if matches!(self.state, State::SignedOut(None) | State::NeedsSignIn(_)) =>
            {
                self.advance();
                self.state = State::Requesting;
                effects.push(Effect::RequestCode { stamp: self.stamp });
            }
            Input::Op(GitHubOp::RetryCleanup)
                if matches!(
                    self.state,
                    State::SignedOut(Some(Cleanup::Refused | Cleanup::NotDurable))
                ) =>
            {
                self.state = State::SignedOut(Some(Cleanup::Running));
                effects.push(Effect::Remove {
                    generation: self.stamp.generation,
                });
            }
            Input::Removed { generation, result }
                if generation == self.stamp.generation
                    && matches!(self.state, State::SignedOut(Some(_))) =>
            {
                self.state = State::SignedOut(cleanup(result));
            }
            Input::Loaded { stamp, result }
                if stamp == self.stamp && matches!(self.state, State::Loading) =>
            {
                match result {
                    Ok(Some(account)) => {
                        effects.push(Effect::ReadUser {
                            stamp,
                            token: account.token.clone(),
                        });
                        self.state = State::Verifying {
                            token: account.token,
                            loaded_user: Some(account.user),
                        };
                    }
                    Ok(None) => self.state = State::SignedOut(None),
                    Err(result) => self.state = State::SignedOut(cleanup(result)),
                }
            }
            Input::Code { stamp, result }
                if stamp == self.stamp && matches!(self.state, State::Requesting) =>
            {
                match result {
                    Ok(challenge) => {
                        let deadline = now + Duration::from_secs(u64::from(challenge.expires_in_s));
                        let next_poll = now + Duration::from_secs(u64::from(challenge.interval_s));
                        self.deadline = Some(deadline);
                        self.state = State::Awaiting(Attempt {
                            challenge,
                            deadline,
                            next_poll,
                            polling: false,
                        });
                        effects.push(Effect::Wake(next_poll.min(deadline)));
                    }
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::Tick => {
                if let State::Awaiting(a) = &mut self.state
                    && !a.polling
                    && now >= a.next_poll
                {
                    a.polling = true;
                    effects.push(Effect::Poll {
                        stamp: self.stamp,
                        code: a.challenge.code.clone(),
                    });
                    effects.push(Effect::Wake(a.deadline));
                }
            }
            Input::Poll { stamp, result }
                if stamp == self.stamp
                    && matches!(&self.state, State::Awaiting(a) if a.polling) =>
            {
                match result {
                    Ok(TokenReply::Token(token)) => {
                        self.state = State::Verifying {
                            token: token.clone(),
                            loaded_user: None,
                        };
                        effects.push(Effect::ReadUser { stamp, token });
                    }
                    Ok(TokenReply::Error {
                        code: code @ (TokenError::AuthorizationPending | TokenError::SlowDown),
                        interval_s,
                    }) => {
                        if let State::Awaiting(a) = &mut self.state {
                            if code == TokenError::SlowDown {
                                a.challenge.interval_s = (a.challenge.interval_s + 5)
                                    .max(interval_s.unwrap_or(0))
                                    .min(900);
                            }
                            a.polling = false;
                            a.next_poll =
                                now + Duration::from_secs(u64::from(a.challenge.interval_s));
                            effects.push(Effect::Wake(a.next_poll.min(a.deadline)));
                        }
                    }
                    Ok(TokenReply::Error { code, .. }) => self.fail(token_key(code), &mut effects),
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::User { stamp, result }
                if stamp == self.stamp && matches!(self.state, State::Verifying { .. }) =>
            {
                let State::Verifying { token, loaded_user } =
                    std::mem::replace(&mut self.state, State::SignedOut(None))
                else {
                    unreachable!()
                };
                match result {
                    Ok(user) => {
                        let account = Account { token, user };
                        if loaded_user.is_some() {
                            self.advance();
                            self.state = State::Connected(account);
                            effects.push(Effect::RefreshRepos {
                                generation: self.stamp.generation,
                            });
                        } else {
                            effects.push(Effect::Save {
                                stamp,
                                account: Account {
                                    token: account.token.clone(),
                                    user: GitHubUser {
                                        id: account.user.id,
                                        login: account.user.login.clone(),
                                    },
                                },
                            });
                            self.state = State::Saving(account);
                        }
                    }
                    Err(ApiError::Refused { status: 401, .. }) if loaded_user.is_some() => {
                        self.advance();
                        self.state = State::NeedsSignIn(
                            loaded_user.expect("guard requires loaded user").login,
                        );
                    }
                    Err(error) if loaded_user.is_some() => {
                        self.advance();
                        self.state = State::Connected(Account {
                            token,
                            user: loaded_user.expect("guard requires loaded user"),
                        });
                        effects.push(Effect::Error(api_key(error).into()));
                    }
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::Saved { stamp, saved }
                if stamp == self.stamp && matches!(self.state, State::Saving(_)) =>
            {
                if saved {
                    if let State::Saving(account) =
                        std::mem::replace(&mut self.state, State::SignedOut(None))
                    {
                        self.advance();
                        self.state = State::Connected(account);
                        effects.push(Effect::RefreshRepos {
                            generation: self.stamp.generation,
                        });
                    }
                } else {
                    self.fail(ErrorKey::CredentialsUnavailable, &mut effects);
                }
            }
            Input::Unauthorized { generation }
                if generation == self.stamp.generation
                    && matches!(self.state, State::Connected(_)) =>
            {
                if let State::Connected(account) =
                    std::mem::replace(&mut self.state, State::SignedOut(None))
                {
                    self.advance();
                    self.state = State::NeedsSignIn(account.user.login);
                    effects.push(Effect::CancelAll);
                }
            }
            Input::Repos {
                generation,
                names,
                truncated,
            } if generation == self.stamp.generation
                && matches!(self.state, State::Connected(_)) =>
            {
                self.repos = names;
                self.truncated = truncated;
            }
            Input::Op(GitHubOp::RefreshRepos) if matches!(self.state, State::Connected(_)) => {
                effects.push(Effect::RefreshRepos {
                    generation: self.stamp.generation,
                });
            }
            Input::Op(GitHubOp::SetDefaultRepo(repo))
                if matches!(self.state, State::Connected(_)) =>
            {
                if let Some(name) = self.repos.iter().find(|name| *name == &repo) {
                    self.default = Some(name.clone());
                    effects.push(Effect::SaveDefault(name.clone()));
                } else {
                    effects.push(Effect::Error(ErrorKey::RequestRefused.into()));
                }
            }
            _ => {}
        }
        effects.push(Effect::View(self.view(now)));
        effects
    }
}

fn cleanup(result: Removed) -> Option<Cleanup> {
    match result {
        Removed::Clean => None,
        Removed::Refused => Some(Cleanup::Refused),
        Removed::NotDurable => Some(Cleanup::NotDurable),
    }
}
fn token_key(code: TokenError) -> ErrorKey {
    match code {
        TokenError::ExpiredToken => ErrorKey::SignInTimedOut,
        TokenError::DeviceFlowDisabled
        | TokenError::IncorrectClientCredentials
        | TokenError::UnsupportedGrantType => ErrorKey::AccountMisconfigured,
        _ => ErrorKey::SignInFailed,
    }
}
fn api_key(error: ApiError) -> ErrorKey {
    match error {
        ApiError::Http(_) => ErrorKey::AccountOffline,
        ApiError::RefreshUnsupported => ErrorKey::AccountMisconfigured,
        _ => ErrorKey::ResponseUnreadable,
    }
}
