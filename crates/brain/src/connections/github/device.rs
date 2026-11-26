//! GitHub device sign-in: inputs in, effects out; no requests or credential I/O.

use super::{
    DeviceCodeValue, GitHubToken,
    api::{
        ApiError, DeviceCode, GitHubUser, MAX_ACCESS_LIFETIME_S, MAX_REFRESH_LIFETIME_S,
        TokenError, TokenReply, Tokens,
    },
};
use crate::config::tuning::TokenTuning;
use crate::connections::clock::{Clock, SystemClock};
use kumochi_protocol::{
    Secret,
    message::{
        Cleanup, ErrorKey, GitHubLogin, GitHubOp, GitHubView, MAX_GITHUB_REPOS, RepoName, UserError,
    },
};
use kumochi_scene::text::Bounded;
use std::{
    fmt,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

/// Identifies the admission epoch for both an attempt and its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Monotonic local credential-use fence.
    pub generation: u64,
}

/// A checked account awaiting durable ownership.
#[derive(Clone)]
pub struct Account {
    /// The pair is replaced together through the credential FIFO.
    pub credentials: Tokens,
    /// Identity verified by the authenticated user endpoint.
    pub user: GitHubUser,
    /// Persisted before dispatch so a lost refresh is never replayed after restart.
    pub refresh_sent: bool,
}
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubAccount")
    }
}

/// A non-secret default scoped to one verified numeric account.
#[derive(Clone)]
pub struct DefaultRepo {
    /// The account that selected this repository.
    pub account: u64,
    /// GitHub's checked repository spelling.
    pub repo: RepoName,
}

impl fmt::Debug for DefaultRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubDefaultRepo")
    }
}

/// A complete bounded listing, never a partial authority snapshot.
#[derive(Clone)]
pub struct Repositories {
    /// Names in GitHub's spelling.
    pub names: Bounded<RepoName, MAX_GITHUB_REPOS>,
    /// GitHub's numeric id for each name, in the same order.
    pub ids: Vec<NonZeroU64>,
}

impl Repositories {
    /// GitHub's id for `name`, if the listing has both.
    pub fn id(&self, name: &RepoName) -> Option<NonZeroU64> {
        id_of(&self.names, &self.ids, name)
    }
}

/// The id listed beside `name` in a names-and-ids pair of lists.
pub(crate) fn id_of(names: &[RepoName], ids: &[NonZeroU64], name: &RepoName) -> Option<NonZeroU64> {
    names
        .iter()
        .position(|listed| listed == name)
        .and_then(|at| ids.get(at))
        .copied()
}

impl fmt::Debug for Repositories {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubRepositories")
            .field("count", &self.names.len())
            .field("ids", &self.ids.len())
            .finish()
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
        /// Account or cleanup outcome; an unavailable vault does not supply this input.
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
    /// One single-use refresh finished.
    Refreshed {
        /// Rotation identity.
        stamp: Stamp,
        /// A complete expiring pair, or a terminal refusal.
        result: Result<Tokens, ApiError>,
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
        /// Only a complete successful listing can grant authority.
        result: Result<Repositories, ApiError>,
    },
    /// The ordered settings writer settled a default change.
    DefaultSaved {
        /// The account generation that selected the repository.
        generation: u64,
        /// The proposed spelling.
        repo: RepoName,
        /// True only after the file was committed.
        saved: bool,
    },
    /// A consumer received 401.
    Unauthorized {
        /// Consumer fence.
        generation: u64,
    },
    /// An owned task ended without an API result.
    Stopped {
        /// The operation that ended; a stale one cannot end its replacement.
        stamp: Stamp,
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
    /// Rotate a refresh token whose single use was durably recorded first.
    RefreshToken {
        /// Completion identity.
        stamp: Stamp,
        /// The one secret to send; no client secret is used in device flow.
        token: Secret<super::RefreshToken>,
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
    SaveDefault {
        /// Completion fence.
        generation: u64,
        /// Non-secret account-bound metadata.
        default: DefaultRepo,
    },
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
        credentials: Tokens,
        loaded_user: Option<GitHubUser>,
        rotated: bool,
        listing: OnConnect,
    },
    Saving {
        account: Account,
        next: AfterSave,
    },
    Refreshing(Account),
    Connected(Account),
    NeedsSignIn(GitHubLogin),
}

#[derive(Clone, Copy)]
enum AfterSave {
    Connect,
    Refresh,
    Rotated,
}

#[derive(Clone, Copy)]
enum OnConnect {
    Defer,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    Unlisted,
    Fetching,
    Listed,
}

/// Owns the one attempt, bearer and authoritative connection generation.
pub struct Machine {
    state: State,
    stamp: Stamp,
    fence: Arc<AtomicU64>,
    deadline: Option<Instant>,
    repos: Bounded<RepoName, MAX_GITHUB_REPOS>,
    repo_ids: Vec<NonZeroU64>,
    truncated: bool,
    default: Option<DefaultRepo>,
    pending_default: Option<RepoName>,
    verified: bool,
    listing: Listing,
    token_end: Option<Instant>,
    refresh_lead: Duration,
    refresh_due: Option<Instant>,
    clock: Arc<dyn Clock>,
}
impl fmt::Debug for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubMachine")
    }
}

impl Machine {
    /// Starts fenced until Loaded arrives. Queue the startup load with `stamp()`.
    pub fn new(default: Option<DefaultRepo>) -> Self {
        Self::with_refresh_lead(default, TokenTuning::default().github_refresh_lead)
    }

    pub(crate) fn with_refresh_lead(default: Option<DefaultRepo>, refresh_lead: Duration) -> Self {
        Self {
            state: State::Loading,
            stamp: Stamp { generation: 0 },
            fence: Arc::new(AtomicU64::new(0)),
            deadline: None,
            repos: Bounded::default(),
            repo_ids: Vec::new(),
            truncated: false,
            default,
            pending_default: None,
            verified: false,
            listing: Listing::Unlisted,
            token_end: None,
            refresh_lead,
            refresh_due: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// Identity to attach to newly dispatched work.
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    #[cfg(test)]
    pub(crate) fn clock_for_test(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self.state, State::Loading)
    }

    pub(crate) fn connecting(&self) -> bool {
        self.deadline.is_some() || matches!(self.state, State::Requesting)
    }

    pub(crate) fn connect_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Consumers load this with Acquire immediately before using a bearer or saving.
    pub fn fence(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.fence)
    }

    /// The verified connected user, without its bearer: `Some` only while Connected.
    pub fn user(&self) -> Option<&GitHubUser> {
        match &self.state {
            State::Connected(account)
                if self.verified
                    && self.token_end.is_some_and(|end| Instant::now() < end)
                    && self
                        .remaining(account.credentials.expires_at_s, MAX_ACCESS_LIFETIME_S)
                        .is_some() =>
            {
                Some(&account.user)
            }
            _ => None,
        }
    }

    /// Restores only committed non-secret configuration; the view checks its owner.
    pub(crate) fn restore_default(&mut self, default: Option<DefaultRepo>) {
        self.default = default;
    }

    /// A transient bearer for the current verified account generation.
    pub(crate) fn token(&self) -> Option<Secret<GitHubToken>> {
        self.user()?;
        match &self.state {
            State::Connected(account) => Some(account.credentials.token.clone()),
            _ => None,
        }
    }

    pub(crate) fn issue_access(
        &self,
        account: u64,
        repo: &RepoName,
    ) -> Option<(Secret<GitHubToken>, Instant, u64)> {
        if self.user()?.id != account || !self.repositories()?.contains(repo) {
            return None;
        }
        let State::Connected(connected) = &self.state else {
            return None;
        };
        Some((
            connected.credentials.token.clone(),
            self.token_end?,
            connected.credentials.expires_at_s,
        ))
    }

    /// Complete repository authority for consumers in this generation.
    pub(crate) fn repositories(&self) -> Option<&[RepoName]> {
        self.user()?;
        (self.listing == Listing::Listed).then_some(self.repos.as_ref())
    }

    pub(crate) fn can_list_repositories(&self) -> bool {
        self.listing == Listing::Unlisted && self.user().is_some()
    }

    /// GitHub's id for each of [`Self::repositories`], in the same order.
    pub(crate) fn repository_ids(&self) -> Option<&[NonZeroU64]> {
        self.repositories()?;
        Some(&self.repo_ids)
    }

    pub(crate) fn rotating(&self) -> bool {
        matches!(
            self.state,
            State::Refreshing(_)
                | State::Verifying { rotated: true, .. }
                | State::Saving {
                    next: AfterSave::Refresh | AfterSave::Rotated,
                    ..
                }
        )
    }

    /// A projection with no bearer, device credential or provider URL.
    pub fn view(&self, now: Instant) -> GitHubView {
        match &self.state {
            State::Loading => GitHubView::SignedOut { cleanup: None },
            State::SignedOut(cleanup) => GitHubView::SignedOut { cleanup: *cleanup },
            State::Requesting
            | State::Verifying { .. }
            | State::Saving { .. }
            | State::Refreshing(_) => GitHubView::Requesting,
            State::Awaiting(a) => GitHubView::AwaitingUser {
                user_code: a.challenge.user_code.clone(),
                expires_in_s: u16::try_from(
                    self.deadline
                        .unwrap_or(now)
                        .saturating_duration_since(now)
                        .as_secs(),
                )
                .unwrap_or(900),
            },
            State::Connected(account) if !self.verified => GitHubView::NotVerified {
                login: account.user.login.clone(),
            },
            State::Connected(account) if self.listing != Listing::Listed => {
                GitHubView::ConnectedUnlisted {
                    login: account.user.login.clone(),
                    listing: self.listing == Listing::Fetching,
                }
            }
            State::Connected(account) => GitHubView::Connected {
                login: account.user.login.clone(),
                repos: self.repos.clone(),
                truncated: self.truncated,
                default: self
                    .default
                    .as_ref()
                    .filter(|default| {
                        default.account == account.user.id && self.repos.contains(&default.repo)
                    })
                    .map(|default| default.repo.clone()),
            },
            State::NeedsSignIn(login) => GitHubView::NeedsSignIn {
                login: login.clone(),
            },
        }
    }

    fn advance(&mut self) {
        self.stamp.generation = self
            .stamp
            .generation
            .checked_add(1)
            .expect("generation counter exhausted");
        self.fence.store(self.stamp.generation, Ordering::Release);
        self.deadline = None;
        self.repos = Bounded::new(Vec::new()).expect("empty repository list is bounded");
        self.repo_ids.clear();
        self.truncated = false;
        self.verified = false;
        self.listing = Listing::Unlisted;
        self.pending_default = None;
        self.token_end = None;
        self.refresh_due = None;
    }

    fn fail(&mut self, key: ErrorKey, effects: &mut Vec<Effect>) {
        self.cancel(effects);
        effects.push(Effect::Error(key.into()));
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) {
        if matches!(
            self.state,
            State::Verifying {
                loaded_user: Some(_),
                rotated: false,
                ..
            }
        ) {
            let State::Verifying {
                credentials,
                loaded_user: Some(user),
                ..
            } = std::mem::replace(&mut self.state, State::SignedOut(None))
            else {
                unreachable!()
            };
            self.not_verified(
                Account {
                    credentials,
                    user,
                    refresh_sent: false,
                },
                effects,
            );
            return;
        }
        let saving = matches!(self.state, State::Saving { .. });
        self.advance();
        self.state = State::SignedOut(saving.then_some(Cleanup::Running));
        effects.push(Effect::CancelAll);
        if saving {
            effects.push(Effect::Remove {
                generation: self.stamp.generation,
            });
        }
    }

    fn remaining(&self, expires_at: u64, maximum: u64) -> Option<Duration> {
        expires_at
            .checked_sub(self.clock.unix_seconds()?)
            .filter(|seconds| (1..=maximum).contains(seconds))
            .map(Duration::from_secs)
    }

    fn needs_sign_in(&mut self, login: GitHubLogin, effects: &mut Vec<Effect>) {
        self.advance();
        self.state = State::NeedsSignIn(login);
        effects.push(Effect::CancelAll);
    }

    fn connected(
        &mut self,
        account: Account,
        now: Instant,
        listing: OnConnect,
        effects: &mut Vec<Effect>,
    ) {
        let Some(remaining) =
            self.remaining(account.credentials.expires_at_s, MAX_ACCESS_LIFETIME_S)
        else {
            self.needs_sign_in(account.user.login, effects);
            return;
        };
        self.advance();
        self.token_end = Some(now + remaining);
        let lead = self.refresh_lead.min(remaining / 2);
        self.refresh_due = Some(now + remaining.saturating_sub(lead));
        self.verified = true;
        self.state = State::Connected(account);
        if matches!(listing, OnConnect::List) {
            self.list_repositories(effects);
        }
        effects.extend(self.refresh_due.map(Effect::Wake));
    }

    fn list_repositories(&mut self, effects: &mut Vec<Effect>) {
        self.repos = Bounded::default();
        self.repo_ids.clear();
        self.listing = Listing::Fetching;
        effects.push(Effect::RefreshRepos {
            generation: self.stamp.generation,
        });
    }

    fn not_verified(&mut self, account: Account, effects: &mut Vec<Effect>) {
        self.advance();
        self.state = State::Connected(account);
        effects.push(Effect::CancelAll);
    }

    fn start_refresh(&mut self, effects: &mut Vec<Effect>) {
        let State::Connected(mut account) =
            std::mem::replace(&mut self.state, State::SignedOut(None))
        else {
            return;
        };
        if account.refresh_sent
            || self
                .remaining(
                    account.credentials.refresh_expires_at_s,
                    MAX_REFRESH_LIFETIME_S,
                )
                .is_none()
        {
            self.needs_sign_in(account.user.login, effects);
            return;
        }
        self.advance();
        account.refresh_sent = true;
        effects.push(Effect::CancelAll);
        effects.push(Effect::Save {
            stamp: self.stamp,
            account: account.clone(),
        });
        self.state = State::Saving {
            account,
            next: AfterSave::Refresh,
        };
    }

    /// Apply one event at the caller's clock. Ended attempts cannot be revived by completions.
    ///
    /// # Panics
    /// If a process exhausts its 64-bit generation counter.
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
                if self.connecting()
                    || self.rotating()
                    || matches!(self.state, State::Verifying { .. }) =>
            {
                self.cancel(&mut effects);
            }
            Input::Op(GitHubOp::Connect)
                if matches!(
                    self.state,
                    State::SignedOut(None | Some(Cleanup::Refused)) | State::NeedsSignIn(_)
                ) =>
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
                        if account.refresh_sent
                            || self
                                .remaining(
                                    account.credentials.refresh_expires_at_s,
                                    MAX_REFRESH_LIFETIME_S,
                                )
                                .is_none()
                        {
                            self.needs_sign_in(account.user.login, &mut effects);
                        } else if self
                            .remaining(account.credentials.expires_at_s, MAX_ACCESS_LIFETIME_S)
                            .is_none()
                        {
                            self.state = State::Connected(account);
                            self.start_refresh(&mut effects);
                        } else {
                            effects.push(Effect::ReadUser {
                                stamp,
                                token: account.credentials.token.clone(),
                            });
                            self.state = State::Verifying {
                                credentials: account.credentials,
                                loaded_user: Some(account.user),
                                rotated: false,
                                listing: OnConnect::Defer,
                            };
                        }
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
                            next_poll,
                            polling: false,
                        });
                        effects.push(Effect::Wake(next_poll.min(deadline)));
                    }
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::Tick => {
                if matches!(self.state, State::Connected(_))
                    && self.refresh_due.is_some_and(|due| now >= due)
                {
                    self.start_refresh(&mut effects);
                }
                if let State::Awaiting(a) = &mut self.state
                    && !a.polling
                    && now >= a.next_poll
                {
                    a.polling = true;
                    effects.push(Effect::Poll {
                        stamp: self.stamp,
                        code: a.challenge.code.clone(),
                    });
                    effects.extend(self.deadline.map(Effect::Wake));
                }
            }
            Input::Poll { stamp, result }
                if stamp == self.stamp
                    && matches!(&self.state, State::Awaiting(a) if a.polling) =>
            {
                match result {
                    Ok(TokenReply::Token(credentials)) => {
                        let token = credentials.token.clone();
                        self.state = State::Verifying {
                            credentials,
                            loaded_user: None,
                            rotated: false,
                            listing: OnConnect::Defer,
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
                            effects.extend(
                                self.deadline
                                    .map(|deadline| Effect::Wake(a.next_poll.min(deadline))),
                            );
                        }
                    }
                    Ok(TokenReply::Error { code, .. }) => self.fail(token_key(code), &mut effects),
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::Refreshed { stamp, result }
                if stamp == self.stamp && matches!(self.state, State::Refreshing(_)) =>
            {
                let State::Refreshing(account) =
                    std::mem::replace(&mut self.state, State::SignedOut(None))
                else {
                    unreachable!()
                };
                match result {
                    Ok(credentials) => {
                        effects.push(Effect::ReadUser {
                            stamp,
                            token: credentials.token.clone(),
                        });
                        self.state = State::Verifying {
                            credentials,
                            loaded_user: Some(account.user),
                            rotated: true,
                            listing: OnConnect::Defer,
                        };
                    }
                    Err(error) => {
                        self.needs_sign_in(account.user.login, &mut effects);
                        effects.push(Effect::Error(api_key(error).into()));
                    }
                }
            }
            Input::User { stamp, result }
                if stamp == self.stamp && matches!(self.state, State::Verifying { .. }) =>
            {
                let State::Verifying {
                    credentials,
                    loaded_user,
                    rotated,
                    listing,
                } = std::mem::replace(&mut self.state, State::SignedOut(None))
                else {
                    unreachable!()
                };
                match result {
                    Ok(user)
                        if loaded_user
                            .as_ref()
                            .is_some_and(|loaded| loaded.id != user.id) =>
                    {
                        self.needs_sign_in(
                            loaded_user.expect("guard requires loaded user").login,
                            &mut effects,
                        );
                        effects.push(Effect::Error(ErrorKey::RequestRefused.into()));
                    }
                    Ok(user) => {
                        let account = Account {
                            credentials,
                            user,
                            refresh_sent: false,
                        };
                        if loaded_user.is_some() && !rotated {
                            self.connected(account, now, listing, &mut effects);
                        } else {
                            effects.push(Effect::Save {
                                stamp,
                                account: account.clone(),
                            });
                            self.state = State::Saving {
                                account,
                                next: if rotated {
                                    AfterSave::Rotated
                                } else {
                                    AfterSave::Connect
                                },
                            };
                        }
                    }
                    Err(error)
                        if loaded_user.is_some()
                            && (rotated
                                || matches!(error, ApiError::Refused { status: 401, .. })) =>
                    {
                        self.needs_sign_in(
                            loaded_user.expect("guard requires loaded user").login,
                            &mut effects,
                        );
                        effects.push(Effect::Error(api_key(error).into()));
                    }
                    Err(error) if loaded_user.is_some() => {
                        self.not_verified(
                            Account {
                                credentials,
                                user: loaded_user.expect("guard requires loaded user"),
                                refresh_sent: false,
                            },
                            &mut effects,
                        );
                        effects.push(Effect::Error(api_key(error).into()));
                    }
                    Err(error) => self.fail(api_key(error), &mut effects),
                }
            }
            Input::Saved { stamp, saved }
                if stamp == self.stamp && matches!(self.state, State::Saving { .. }) =>
            {
                let State::Saving { account, next } =
                    std::mem::replace(&mut self.state, State::SignedOut(None))
                else {
                    unreachable!()
                };
                if saved {
                    match next {
                        AfterSave::Connect | AfterSave::Rotated => {
                            let listing = if matches!(next, AfterSave::Connect) {
                                OnConnect::List
                            } else {
                                OnConnect::Defer
                            };
                            self.connected(account, now, listing, &mut effects);
                        }
                        AfterSave::Refresh => {
                            effects.push(Effect::RefreshToken {
                                stamp,
                                token: account.credentials.refresh.clone(),
                            });
                            self.state = State::Refreshing(account);
                        }
                    }
                } else {
                    self.state = State::Saving { account, next };
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
                    self.needs_sign_in(account.user.login, &mut effects);
                }
            }
            Input::Stopped { stamp } if stamp == self.stamp => {
                let login = match &self.state {
                    State::Verifying {
                        loaded_user: Some(user),
                        ..
                    } => Some(user.login.clone()),
                    State::Connected(account) | State::Refreshing(account) => {
                        Some(account.user.login.clone())
                    }
                    _ => None,
                };
                if let Some(login) = login {
                    self.needs_sign_in(login, &mut effects);
                    effects.push(Effect::Error(ErrorKey::RequestRefused.into()));
                } else {
                    self.fail(ErrorKey::RequestRefused, &mut effects);
                }
            }
            Input::Repos { generation, result }
                if generation == self.stamp.generation
                    && matches!(self.state, State::Connected(_)) =>
            {
                self.repos = Bounded::default();
                self.repo_ids.clear();
                self.listing = Listing::Unlisted;
                match result {
                    Ok(repositories) if self.verified => {
                        self.repos = repositories.names;
                        self.repo_ids = repositories.ids;
                        self.listing = Listing::Listed;
                    }
                    Ok(_) => {}
                    Err(ApiError::Refused { status: 401, .. }) => {
                        if let State::Connected(account) =
                            std::mem::replace(&mut self.state, State::SignedOut(None))
                        {
                            self.needs_sign_in(account.user.login, &mut effects);
                        }
                    }
                    Err(error) => effects.push(Effect::Error(api_key(error).into())),
                }
            }
            Input::Op(GitHubOp::ListReposIfUnlisted) if self.can_list_repositories() => {
                self.list_repositories(&mut effects);
            }
            Input::Op(GitHubOp::RefreshRepos) if matches!(self.state, State::Connected(_)) => {
                if self.verified {
                    if self.listing != Listing::Fetching {
                        self.list_repositories(&mut effects);
                    }
                } else if let State::Connected(account) =
                    std::mem::replace(&mut self.state, State::SignedOut(None))
                {
                    effects.push(Effect::ReadUser {
                        stamp: self.stamp,
                        token: account.credentials.token.clone(),
                    });
                    self.state = State::Verifying {
                        credentials: account.credentials,
                        loaded_user: Some(account.user),
                        rotated: false,
                        listing: OnConnect::List,
                    };
                }
            }
            Input::Op(GitHubOp::SetDefaultRepo(repo))
                if matches!(self.state, State::Connected(_)) =>
            {
                if self.pending_default.is_none()
                    && let Some(user) = self.user()
                    && let Some(name) = self
                        .repositories()
                        .and_then(|repos| repos.iter().find(|name| *name == &repo))
                {
                    let default = DefaultRepo {
                        account: user.id,
                        repo: name.clone(),
                    };
                    self.pending_default = Some(name.clone());
                    effects.push(Effect::SaveDefault {
                        generation: self.stamp.generation,
                        default,
                    });
                } else {
                    effects.push(Effect::Error(ErrorKey::RequestRefused.into()));
                }
            }
            Input::DefaultSaved {
                generation,
                repo,
                saved,
            } if generation == self.stamp.generation
                && self.pending_default.as_ref() == Some(&repo) =>
            {
                self.pending_default = None;
                if saved {
                    if let Some(user) = self.user() {
                        self.default = Some(DefaultRepo {
                            account: user.id,
                            repo,
                        });
                    }
                } else {
                    effects.push(Effect::Error(ErrorKey::SettingsRefused.into()));
                }
            }
            _ => {}
        }
        if !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SaveDefault { .. }))
        {
            effects.push(Effect::View(self.view(now)));
        }
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
        _ => ErrorKey::ResponseUnreadable,
    }
}
