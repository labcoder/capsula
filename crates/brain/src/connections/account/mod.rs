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
mod refresh;
mod sign_out;
mod sweep;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    AccountEmail, AccountOp, AccountRecovery, AccountView, Action, ErrorKey, PlanState, UserError,
};
use zeroize::Zeroizing;

pub use job::{Done, Job, Snapshot, Worker};
use lane::{Deferred, Flight, Lane, Queued, Save, Waiting, listed};
use sign_out::Leaving;
use sweep::{Pass, Why};

use super::http_client::HttpError;
use super::oauth::exchange::{ExchangeError, OAuthCode, TokenSet};
use super::oauth::id_token::{IdentityError, VerifiedIdentity, check_subject, permits_plan};
use super::oauth::{Authorization, BindError, Intent, LaunchError, SignInError};
use super::records::{Account, MAX_EXCHANGES, Origin, Record as _, Revocable, Tokens, entry_fits};

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
    /// A revocation run ended with these entries confirmed.
    Revoked(Vec<Secret<Revocable>>),
    /// A caller wants the bearer, refreshed first if it's due.
    Access {
        /// Names the caller's answer.
        ticket: u64,
        /// Wall time, for the expiry check.
        now: Option<u64>,
    },
    /// The refresh for `generation` ended.
    Refreshed {
        /// The account generation it was sent for.
        generation: u64,
        /// The rotated set, or why there's none.
        result: Result<TokenSet, ExchangeError>,
        /// Wall time when it arrived, for its expiry.
        now: Option<u64>,
    },
    /// The identity check of `generation`'s rotated set ended.
    Validated {
        /// The account generation it checked.
        generation: u64,
        /// The verified identity, or why it isn't.
        result: Result<VerifiedIdentity, IdentityError>,
    },
}

/// Why [`Input::Access`] gave no bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// No usable account: signed out, signing out or busy, or the account the caller asked
    /// about was replaced while it waited.
    Unusable,
    /// The provider or the store refused; this is what to tell the user.
    Failed(UserError),
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
    /// Revoke these listed entries within one network budget, then report [`Input::Revoked`].
    Revoke(Vec<Secret<Revocable>>),
    /// Send the refresh, then report [`Input::Refreshed`]. Never aborted once started.
    Refresh {
        /// The account generation it's for.
        generation: u64,
        /// The issued client.
        client: Secret<Zeroizing<String>>,
        /// The account's current refresh token.
        refresh_token: Secret<Zeroizing<String>>,
    },
    /// Verify a rotated set's identity token, then report [`Input::Validated`].
    Validate {
        /// The account generation it's for.
        generation: u64,
        /// The issued client.
        client: Secret<Zeroizing<String>>,
        /// The subject this account already verified.
        subject: Secret<Zeroizing<String>>,
        /// The rotated identity token.
        id_token: Secret<Zeroizing<String>>,
    },
    /// Answer an [`Input::Access`].
    Access {
        /// The caller's ticket.
        ticket: u64,
        /// The bearer, or why there's none.
        result: Result<Secret<Zeroizing<String>>, Denied>,
    },
}

/// The account's state as last read, and what the machine learned since.
#[derive(Debug, Default)]
struct Loaded {
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
    /// Names the account's token set; it moves when that set is replaced or removed.
    generation: u64,
    attempt: u64,
    live: Option<Live>,
    deferred: Option<Deferred>,
    lane: Lane,
    waiting: VecDeque<Waiting>,
    /// The generation a refresh flight serves, from its request until its set is verified.
    refreshing: Option<u64>,
    /// The generation whose rotated set is being verified.
    validating: Option<u64>,
    /// Callers waiting for the refresh flight.
    tickets: Vec<u64>,
    jobs: VecDeque<Queued>,
    flight: Option<Flight>,
    shown: Option<AccountView>,
    /// The revocation pass outside a sign-out, and whether another is owed after it.
    pass: Pass,
    again: bool,
    /// A sign-in that found the list full, waiting for its age-out check.
    admitting: Option<Intent>,
    /// A pending sign-out: the account is fenced.
    leaving: Option<Leaving>,
}

impl AccountState {
    /// A machine that knows nothing yet; its first effect is the patient startup load.
    #[must_use]
    pub fn start() -> (Self, Vec<Effect>) {
        let mut state = Self {
            busy: true,
            loading: false,
            loaded: Loaded::default(),
            generation: 0,
            attempt: 0,
            live: None,
            deferred: None,
            lane: Lane::Free,
            waiting: VecDeque::new(),
            refreshing: None,
            validating: None,
            tickets: Vec::new(),
            jobs: VecDeque::new(),
            flight: None,
            shown: None,
            pass: Pass::Idle,
            again: false,
            admitting: None,
            leaving: None,
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
        if let Some(cleanup) = self.cleanup() {
            return AccountView::SigningOut {
                cleanup,
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
            Input::Revoked(confirmed) => self.revoked(confirmed),
            Input::Access { ticket, now } => self.access(ticket, now, &mut effects),
            Input::Refreshed {
                generation,
                result,
                now,
            } => self.refreshed(generation, result, now, &mut effects),
            Input::Validated { generation, result } => {
                self.validated(generation, result.map(drop), &mut effects);
            }
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
        if self.leaving.is_some() {
            if op == AccountOp::Retry {
                self.retry_sign_out();
            }
            return;
        }
        match op {
            AccountOp::SignIn => self.sign_in(Intent::SignIn, effects),
            AccountOp::EnablePlan => self.sign_in(Intent::EnablePlan, effects),
            AccountOp::CancelSignIn => self.cancel(effects),
            AccountOp::SignOut => self.sign_out(effects),
            AccountOp::Retry => self.retry(),
        }
    }

    /// Starts an attempt; a full list first gets its age-out check.
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
        if self.full() {
            if self.admitting.replace(intent).is_none() {
                self.jobs.push_back(Queued::Relist {
                    confirmed: Vec::new(),
                    why: Why::Admit,
                });
            }
            return;
        }
        self.admitting = None;
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

    /// No list slot is free; the lane's reservation counts, since the request it holds may
    /// still need one.
    fn full(&self) -> bool {
        self.loaded.exchanges + usize::from(!matches!(self.lane, Lane::Free)) >= MAX_EXCHANGES
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) {
        // A sign-in waiting for its age check is cancelled too, before it can start.
        self.admitting = None;
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
        } else {
            self.sweep();
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
            }) if entry_fits(client.expose(), refresh_token.expose()) => {
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
        let refresh_token = match tokens.refresh_token {
            Some(token) if entry_fits(client.expose(), token.expose()) => token,
            // Decision (PR 9b): a grant without a refresh token can't be renewed or revoked, so
            // it's refused and its access token expires on its own. A token no list could hold
            // can't be owned, so its outcome is unknown.
            unowned => {
                self.jobs.push_back(Queued::Finished {
                    outcome_unknown: unowned.is_some(),
                });
                if current {
                    self.end_attempt(ErrorKey::SignInFailed, effects);
                }
                return;
            }
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
                        id_token: tokens.id_token.as_ref().map(|token| token.expose().clone()),
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

    fn loaded(
        &mut self,
        result: Result<Box<Snapshot>, super::error::VaultError>,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = match result {
            Ok(snapshot) => *snapshot,
            Err(super::error::VaultError::Busy) => {
                self.busy = true;
                return;
            }
            Err(failure) => {
                tracing::warn!(%failure, "the account's session lock failed");
                self.busy = false;
                if self.leaving.is_some() {
                    return self.read_refused();
                }
                self.loaded = Loaded {
                    damaged: true,
                    ..Loaded::default()
                };
                return;
            }
        };
        self.busy = false;
        // A load dispatched before a sign-out was admitted can't undo it.
        if self.admitted() {
            return;
        }
        let marker = match snapshot.marker {
            Ok(marker) => marker,
            Err(failure) => return self.unread(&failure),
        };
        self.leaving = None;
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
        let revocable = snapshot
            .revocable
            .inspect_err(|failure| readable("revoke", failure))
            .unwrap_or_default();
        if let Some(sign_out) = marker.sign_out {
            self.resume(sign_out);
        }
        self.loaded = Loaded {
            notice: marker.notice,
            host,
            account,
            exchanges: exchanges(&revocable),
            damaged,
        };
        self.revoke(revocable, effects);
    }
}

fn exchanges(entries: &[Secret<Revocable>]) -> usize {
    entries
        .iter()
        .filter(|entry| entry.expose().origin == Origin::Exchange)
        .count()
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
            code:
                OAuthCode::InvalidGrant
                | OAuthCode::SessionEnded
                | OAuthCode::NotEligible
                | OAuthCode::PlanBusy
                | OAuthCode::GrantRefused
                | OAuthCode::Other,
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
