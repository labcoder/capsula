//! The account state machine (design §3): inputs in, effects out, no I/O. The runtime runs
//! its effects: the listener, the exchange, and credential jobs on one serialized lane.
//!
//! Two kinds of ownership drive it. `attempt` names the one live sign-in; every `SignIn`,
//! `EnablePlan` and `CancelSignIn` moves it. The token lane admits one token request at a time,
//! and a request holds it until its result has a durable owner: the account, the revocation
//! list, or the knowledge that nothing was issued. A received set never has two owners and never
//! none: a stale attempt's set goes to the revocation list, and a set whose save was refused stays
//! in memory, still holding the lane, until a save succeeds.

mod job;
mod lane;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    AccountEmail, AccountOp, AccountRecovery, AccountView, Action, Cleanup, ErrorKey, PlanState,
    UserError,
};
use zeroize::Zeroizing;

pub use job::{Done, Job, Snapshot, Worker};
use lane::{Deferred, Flight, Lane, Queued, Save, listed};

use super::http_client::HttpError;
use super::oauth::exchange::{ExchangeError, OAuthCode, TokenSet};
use super::oauth::id_token::{IdentityError, VerifiedIdentity, check_subject, permits_plan};
use super::oauth::{Authorization, BindError, Intent, LaunchError, SignInError};
use super::records::{Account, MAX_EXCHANGES, MAX_REFRESH_BYTES, Record as _, Tokens};

/// What the account machine reacts to.
#[derive(Debug)]
pub enum Input {
    /// The shell asked for something.
    Op(AccountOp),
    /// A shell connected: a good moment to try a Busy session lock again.
    Connected,
    /// The credential job in flight ended.
    Vault(Done),
    /// An attempt's listener ended.
    Listened {
        /// The attempt it served.
        attempt: u64,
        /// The callback, or why there's none.
        result: Listened,
    },
    /// An attempt's token request, and then its identity check, ended.
    Exchanged {
        /// The attempt it served.
        attempt: u64,
        /// The issued set and its verification, or why there's none.
        result: Result<Received, ExchangeError>,
    },
}

/// How a listener ended.
#[derive(Debug)]
pub enum Listened {
    /// A callback for this attempt.
    Authorized(Authorization),
    /// The loopback couldn't be prepared.
    Bind(BindError),
    /// The browser couldn't be opened.
    Launch(LaunchError),
    /// The callback refused, expired or failed.
    Receive(SignInError),
}

/// A token set the exchange received, with its identity check.
#[derive(Debug)]
pub struct Received {
    /// The issued credentials, still owned by nobody.
    pub tokens: TokenSet,
    /// The ID token's verification.
    pub identity: Result<VerifiedIdentity, IdentityError>,
    /// Wall time when it arrived, for its expiry.
    pub now: Option<u64>,
}

/// What the runtime must do.
#[derive(Debug)]
pub enum Effect {
    /// Tell the shell where the account stands.
    View(AccountView),
    /// Tell the shell something went wrong; it comes before the view it explains.
    Error(UserError),
    /// Run this credential job; the next waits for its [`Done`].
    Vault(Job),
    /// Close any listener and start one for `attempt`.
    Listen {
        /// The attempt.
        attempt: u64,
        /// This machine's host id.
        host: Secret<Zeroizing<String>>,
        /// The saved registration, if any; it never carries tokens.
        saved: Option<Secret<Account>>,
        /// Whether to ask for plan consent.
        intent: Intent,
    },
    /// Close the listener.
    Close,
    /// Send the token request, then verify what comes back. Never aborted once started.
    Exchange {
        /// The attempt.
        attempt: u64,
        /// The callback to exchange.
        authorization: Authorization,
    },
}

/// The account's state as last read, and what the machine learned since.
#[derive(Debug, Default)]
struct Loaded {
    /// A sign-out is pending: the account is fenced.
    fenced: bool,
    /// The provider didn't confirm that the last sign-out ended the session.
    notice: bool,
    host: Option<Secret<Zeroizing<String>>>,
    account: Option<Secret<Account>>,
    /// Exchange entries in the revocation list.
    exchanges: usize,
    /// A record couldn't be read, so no sign-in may write over it.
    damaged: bool,
}

#[derive(Debug)]
enum Step {
    Listening,
    /// The new registration is being saved; its callback waits.
    Registering(Authorization),
    /// Waiting for the lane, or for the exchange.
    Exchanging,
    /// The verified set is being saved as the account.
    Saving,
}

#[derive(Debug)]
struct Live {
    intent: Intent,
    step: Step,
    /// It listened with no saved client, so its callback brings a new registration to save.
    registers: bool,
}

/// A set's subject, display email and expiry, once it may be committed.
type Verified = (Zeroizing<String>, Option<Zeroizing<String>>, u64);

/// The pure account machine.
#[derive(Debug)]
pub struct AccountState {
    busy: bool,
    loading: bool,
    loaded: Loaded,
    attempt: u64,
    live: Option<Live>,
    deferred: Option<Deferred>,
    lane: Lane,
    waiting: VecDeque<(u64, Authorization)>,
    jobs: VecDeque<Queued>,
    flight: Option<Flight>,
    shown: Option<AccountView>,
}

impl AccountState {
    /// A machine that knows nothing yet; its first effect is the patient startup load.
    #[must_use]
    pub fn start() -> (Self, Vec<Effect>) {
        let mut state = Self {
            busy: true,
            loading: false,
            loaded: Loaded::default(),
            attempt: 0,
            live: None,
            deferred: None,
            lane: Lane::Free,
            waiting: VecDeque::new(),
            jobs: VecDeque::new(),
            flight: None,
            shown: None,
        };
        state.loading = true;
        state.jobs.push_back(Queued::Load { patient: true });
        let mut effects = Vec::new();
        state.dispatch(&mut effects);
        (state, effects)
    }

    /// Where the account stands now.
    #[must_use]
    pub fn view(&self) -> AccountView {
        if self.busy {
            return AccountView::Busy;
        }
        if self.loaded.fenced {
            // shortcut: nothing resumes a pending sign-out until slice 3 PR 9b part 2.
            return AccountView::SigningOut {
                cleanup: Cleanup::Refused,
                revocation_unconfirmed: self.loaded.notice,
            };
        }
        if self.live.is_some() {
            return AccountView::SigningIn;
        }
        match self.loaded.account.as_ref().map(Secret::expose) {
            Some(account) if account.tokens.is_some() => AccountView::SignedIn {
                email: account
                    .email
                    .as_ref()
                    .and_then(|email| AccountEmail::new(email).ok()),
                plan: if account.plan_enabled {
                    PlanState::Enabled
                } else {
                    PlanState::Declined
                },
                recovery: self.held().then_some(AccountRecovery::Retry),
            },
            _ => AccountView::SignedOut {
                revocation_unconfirmed: self.loaded.notice,
            },
        }
    }

    /// Applies one input. Every op and every connection gets a view back; anything else sends
    /// one only when it changed.
    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        let answer = matches!(input, Input::Op(_) | Input::Connected);
        match input {
            Input::Op(op) => self.op(op, &mut effects),
            Input::Connected => {
                if self.busy {
                    self.reload();
                }
            }
            Input::Vault(done) => self.done(done, &mut effects),
            Input::Listened { attempt, result } => self.listened(attempt, result, &mut effects),
            Input::Exchanged { attempt, result } => self.exchanged(attempt, result, &mut effects),
        }
        self.pump(&mut effects);
        if answer {
            self.shown = None;
        }
        self.publish(&mut effects);
        effects
    }

    /// Sends the view if it changed since it was last sent.
    fn publish(&mut self, effects: &mut Vec<Effect>) {
        let view = self.view();
        if self.shown.as_ref() != Some(&view) {
            self.shown = Some(view.clone());
            effects.push(Effect::View(view));
        }
    }

    fn op(&mut self, op: AccountOp, effects: &mut Vec<Effect>) {
        if self.busy {
            self.reload();
            return;
        }
        if self.loaded.fenced {
            return;
        }
        match op {
            AccountOp::SignIn => self.sign_in(Intent::SignIn, effects),
            AccountOp::EnablePlan => self.sign_in(Intent::EnablePlan, effects),
            AccountOp::CancelSignIn => self.cancel(effects),
            // shortcut: sign-out arrives with slice 3 PR 9b part 2; until then it's answered
            // with the view, and nothing is fenced or deleted.
            AccountOp::SignOut => {}
            AccountOp::Retry => self.retry(),
        }
    }

    fn sign_in(&mut self, intent: Intent, effects: &mut Vec<Effect>) {
        if self.saving_current() {
            self.deferred = Some(Deferred::SignIn(intent));
            return;
        }
        let host = match &self.loaded.host {
            Some(host) if !self.loaded.damaged => host.clone(),
            _ => return effects.push(error(ErrorKey::CredentialsUnavailable, Action::Retry)),
        };
        // The lane's reservation counts: the request it holds may still need a list slot.
        if self.loaded.exchanges + usize::from(!matches!(self.lane, Lane::Free)) >= MAX_EXCHANGES {
            return effects.push(error(ErrorKey::RevocationBacklog, Action::Retry));
        }
        self.attempt += 1;
        self.live = Some(Live {
            intent,
            step: Step::Listening,
            registers: self.loaded.account.is_none(),
        });
        effects.push(Effect::Listen {
            attempt: self.attempt,
            host,
            saved: self.loaded.account.as_ref().map(|account| {
                let account = account.expose();
                Secret::new(Account {
                    client_id: account.client_id.clone(),
                    subject: None,
                    email: None,
                    plan_enabled: false,
                    tokens: None,
                })
            }),
            intent,
        });
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) {
        if self.saving_current() {
            self.deferred = Some(Deferred::Cancel);
            return;
        }
        let held = self.held() && self.lane_attempt() == Some(self.attempt);
        if self.live.is_none() && !held {
            return;
        }
        // A sent exchange still finishes; its set then goes to the revocation list.
        self.attempt += 1;
        if self.live.take().is_some() {
            effects.push(Effect::Close);
        }
    }

    fn retry(&mut self) {
        if let Lane::Owning {
            held: held @ Some(_),
            ..
        } = &mut self.lane
        {
            *held = None;
            self.jobs.push_back(Queued::Save);
        } else if self.loaded.damaged && self.live.is_none() && matches!(self.lane, Lane::Free) {
            self.reload();
        }
    }

    fn reload(&mut self) {
        if !self.loading {
            self.loading = true;
            self.jobs.push_back(Queued::Load { patient: false });
        }
    }

    fn listened(&mut self, attempt: u64, result: Listened, effects: &mut Vec<Effect>) {
        if attempt != self.attempt
            || !matches!(
                self.live,
                Some(Live {
                    step: Step::Listening,
                    ..
                })
            )
        {
            return;
        }
        let authorization = match result {
            Listened::Authorized(authorization) => authorization,
            Listened::Receive(SignInError::Declined) => {
                self.live = None;
                return;
            }
            Listened::Launch(LaunchError::Launch) => {
                self.live = None;
                return effects.push(error(ErrorKey::BrowserUnavailable, Action::SignIn));
            }
            Listened::Launch(LaunchError::Expired) | Listened::Receive(SignInError::Expired) => {
                self.live = None;
                return effects.push(error(ErrorKey::SignInTimedOut, Action::SignIn));
            }
            Listened::Bind(_) | Listened::Receive(_) => {
                self.live = None;
                return effects.push(error(ErrorKey::SignInFailed, Action::SignIn));
            }
        };
        // Decided by the attempt: a replaced attempt's registration may have landed since.
        if !self.live.as_ref().is_some_and(|live| live.registers) {
            return self.wait_for_lane(authorization);
        }
        let registration = Secret::new(Account {
            client_id: authorization.client_id.expose().clone(),
            subject: None,
            email: None,
            plan_enabled: false,
            tokens: None,
        });
        self.jobs.push_back(Queued::Register {
            attempt,
            account: registration,
        });
        if let Some(live) = &mut self.live {
            live.step = Step::Registering(authorization);
        }
    }

    fn exchanged(
        &mut self,
        attempt: u64,
        result: Result<Received, ExchangeError>,
        effects: &mut Vec<Effect>,
    ) {
        if !matches!(self.lane, Lane::Outstanding { attempt: sent, .. } if sent == attempt) {
            tracing::warn!("an exchange result with no request outstanding for it was ignored");
            return;
        }
        let Lane::Outstanding { client, .. } = mem::replace(&mut self.lane, Lane::Free) else {
            return;
        };
        let current = attempt == self.attempt;
        let received = match result {
            Ok(received) => received,
            // The response was unusable but named a refresh token: the list owns it now.
            Err(ExchangeError::Unusable {
                refresh_token: Some(refresh_token),
            }) if !refresh_token.expose().is_empty()
                && refresh_token.expose().len() <= MAX_REFRESH_BYTES =>
            {
                if current {
                    self.end_attempt(ErrorKey::SignInFailed, effects);
                }
                self.lane = Lane::Owning {
                    attempt,
                    save: listed(client.expose().clone(), refresh_token.expose().clone()),
                    held: None,
                };
                self.jobs.push_back(Queued::Save);
                return;
            }
            Err(failure) => {
                let (outcome_unknown, key) = classify(&failure);
                if outcome_unknown {
                    tracing::warn!(%failure, "a token request's outcome is unknown");
                }
                self.jobs.push_back(Queued::Finished { outcome_unknown });
                if current {
                    self.end_attempt(key, effects);
                }
                return;
            }
        };
        let Received {
            tokens,
            identity,
            now,
        } = received;
        // Decision (PR 9b): a grant without a refresh token can't be renewed or revoked, so
        // it's refused; its access token is dropped and expires on its own.
        let Some(refresh_token) = tokens.refresh_token else {
            self.jobs.push_back(Queued::Finished {
                outcome_unknown: false,
            });
            if current {
                self.end_attempt(ErrorKey::SignInFailed, effects);
            }
            return;
        };
        let verdict = if current {
            self.verdict(
                identity,
                now.and_then(|now| now.checked_add(tokens.expires_in)),
            )
        } else {
            Err(None)
        };
        let save = match verdict {
            Ok((subject, email, expires_at)) => {
                let account = Account {
                    client_id: client.expose().clone(),
                    subject: Some(subject),
                    email,
                    plan_enabled: permits_plan(tokens.scope.expose()),
                    tokens: Some(Tokens {
                        access_token: tokens.access_token.expose().clone(),
                        refresh_token: refresh_token.expose().clone(),
                        id_token: Some(tokens.id_token.expose().clone()),
                        scope: tokens.scope.expose().clone(),
                        expires_at,
                        pending_identity_validation: false,
                    }),
                };
                if account.validate().is_ok() {
                    if let Some(live) = &mut self.live {
                        live.step = Step::Saving;
                    }
                    Save::Account(Secret::new(account))
                } else {
                    self.end_attempt(ErrorKey::SignInFailed, effects);
                    listed(account.client_id, refresh_token.expose().clone())
                }
            }
            Err(key) => {
                if let Some(key) = key {
                    self.end_attempt(key, effects);
                }
                listed(client.expose().clone(), refresh_token.expose().clone())
            }
        };
        self.lane = Lane::Owning {
            attempt,
            save,
            held: None,
        };
        self.jobs.push_back(Queued::Save);
    }

    /// The subject, email and expiry of a set the current attempt may commit, or the error
    /// that sends it to the revocation list.
    fn verdict(
        &self,
        identity: Result<VerifiedIdentity, IdentityError>,
        expires_at: Option<u64>,
    ) -> Result<Verified, Option<ErrorKey>> {
        let identity = identity.map_err(|error| Some(identity_key(error)))?;
        let saved = self
            .loaded
            .account
            .as_ref()
            .and_then(|account| account.expose().subject.as_deref().map(String::as_str));
        check_subject(saved, identity.subject.expose())
            .map_err(|error| Some(identity_key(error)))?;
        let expires_at = expires_at.ok_or(Some(ErrorKey::SignInFailed))?;
        Ok((
            identity.subject.expose().clone(),
            identity.email.map(|email| email.expose().clone()),
            expires_at,
        ))
    }

    fn end_attempt(&mut self, key: ErrorKey, effects: &mut Vec<Effect>) {
        self.live = None;
        let action = match key {
            ErrorKey::AccountMisconfigured => None,
            // An attempt that ended has nothing for Retry to rerun, so a refused registration or
            // pending flag offers a new sign-in.
            ErrorKey::RevocationBacklog => Some(Action::Retry),
            _ => Some(Action::SignIn),
        };
        effects.push(Effect::Error(
            UserError::new(key, Vec::new(), action.into_iter().collect())
                .expect("one action is within the limit"),
        ));
    }

    fn loaded(&mut self, result: Result<Box<Snapshot>, super::error::VaultError>) {
        let snapshot = match result {
            Ok(snapshot) => *snapshot,
            Err(super::error::VaultError::Busy) => {
                self.busy = true;
                return;
            }
            Err(failure) => {
                tracing::warn!(%failure, "the account's session lock failed");
                self.busy = false;
                self.loaded = Loaded {
                    damaged: true,
                    ..Loaded::default()
                };
                return;
            }
        };
        self.busy = false;
        let mut damaged = false;
        let mut readable = |name: &str, failure: &super::error::VaultError| {
            tracing::warn!(%failure, record = name, "an account record can't be read");
            damaged = true;
        };
        let host = snapshot
            .host
            .inspect_err(|failure| readable("host", failure))
            .ok();
        let account = snapshot
            .account
            .inspect_err(|failure| readable("chatgpt", failure))
            .ok()
            .flatten();
        let exchanges = snapshot
            .exchanges
            .inspect_err(|failure| readable("revoke", failure))
            .unwrap_or(0);
        self.loaded = Loaded {
            fenced: snapshot.marker.sign_out.is_some(),
            notice: snapshot.marker.notice,
            host,
            account,
            exchanges,
            damaged,
        };
    }
}

fn error(key: ErrorKey, action: Action) -> Effect {
    Effect::Error(
        UserError::new(key, Vec::new(), vec![action]).expect("one action is within the limit"),
    )
}

/// Whether a failed exchange may have issued tokens, and what the user is told.
fn classify(failure: &ExchangeError) -> (bool, ErrorKey) {
    match failure {
        ExchangeError::Request
        | ExchangeError::Http(
            HttpError::Configuration | HttpError::Endpoint | HttpError::Request,
        )
        | ExchangeError::OAuth {
            code: OAuthCode::InvalidGrant | OAuthCode::Other,
            ..
        } => (false, ErrorKey::SignInFailed),
        ExchangeError::OAuth {
            code: OAuthCode::InvalidClient,
            ..
        } => (false, ErrorKey::AccountMisconfigured),
        ExchangeError::Http(HttpError::TrustStoreUnavailable) => {
            (false, ErrorKey::TrustStoreUnavailable)
        }
        ExchangeError::Http(HttpError::Timeout | HttpError::Transport) => {
            (true, ErrorKey::AccountOffline)
        }
        ExchangeError::Response { status } if *status >= 500 => (true, ErrorKey::AccountOffline),
        ExchangeError::Http(HttpError::Redirect | HttpError::BodyTooLarge)
        | ExchangeError::Response { .. }
        | ExchangeError::Bounds
        | ExchangeError::Unusable { .. } => (true, ErrorKey::SignInFailed),
    }
}

fn identity_key(failure: IdentityError) -> ErrorKey {
    match failure {
        IdentityError::Http(HttpError::Timeout | HttpError::Transport) => ErrorKey::AccountOffline,
        IdentityError::Status { status } if status >= 500 => ErrorKey::AccountOffline,
        IdentityError::OtherAccount => ErrorKey::SignInOtherAccount,
        _ => ErrorKey::SignInFailed,
    }
}
