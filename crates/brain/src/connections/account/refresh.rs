//! Refresh (design §3 rule 3). A caller asks for the bearer; a set within a minute of expiry is
//! refreshed first, on the token lane. The refresh token is marked as sent before it goes out,
//! and one whose answer was never saved is never sent again. The rotated set is saved before
//! anything else goes out, then its identity token is checked against the saved subject. Callers
//! join that one flight until the set is verified. A result for a replaced account never becomes
//! the account.

use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ErrorKey, UserError};
use zeroize::Zeroizing;

use super::lane::{Flight, Lane, Queued, Save, Waiting};
use super::{AccountState, Denied, Effect, Job, classify, identity_key};
use crate::connections::error::{Recovery, VaultError};
use crate::connections::oauth::exchange::{ExchangeError, TokenSet};
use crate::connections::oauth::id_token::{IdentityError, permits_plan};
use crate::connections::records::{Account, Origin, Record as _, Revocable, Tokens, entry_fits};
use crate::connections::vault::SaveResult;

/// Local policy: refresh when the access token has this long left (design §6).
const EARLY_SECONDS: u64 = 60;

/// Which save of a refresh flight a [`Job::Rotate`](super::Job::Rotate) is.
#[derive(Debug, Clone, Copy)]
pub(super) enum Rotation {
    /// The rotated set for a generation; once committed, the lane is free.
    Rotated(u64),
    /// That set once verified, with its pending flag cleared.
    Verified(u64),
    /// The registration a session that ended leaves, for the generation after it.
    Ended(u64),
    /// The account with its refresh token marked as sent, saved before the refresh goes out.
    Marked(u64),
    /// That mark cleared, once nothing was issued for the token.
    Unmarked(u64),
}

impl AccountState {
    pub(super) fn access(&mut self, ticket: u64, now: Option<u64>, effects: &mut Vec<Effect>) {
        self.now = now;
        let tokens = match &self.loaded.account {
            Some(account) if !self.busy && self.leaving.is_none() && !self.session_ended() => {
                account.expose().tokens.as_ref()
            }
            _ => None,
        };
        let Some(pending) = tokens.map(|tokens| tokens.pending_identity_validation) else {
            return effects.push(Effect::Access {
                ticket,
                generation: self.generation,
                result: Err(Denied::Unusable),
            });
        };
        self.tickets.push(ticket);
        if self.refreshing.is_some() {
            return;
        }
        self.refreshing = Some(self.generation);
        // A held save goes again first. A rotated set owned for this account, held or being
        // saved, is never requested again: its commit answers.
        if let Lane::Owning { save, held, .. } = &mut self.lane {
            if held.take().is_some() {
                self.jobs.push_back(Queued::Save);
            }
            if matches!(save, Save::Rotated { generation, .. } if *generation == self.generation) {
                return;
            }
        }
        // A set already being checked answers when its check does.
        if pending {
            if self.validating.is_none() {
                self.validate(effects);
            }
            return;
        }
        self.fresh(effects);
    }

    /// Hands out the bearer, renewing it first when it's due or a provider refused it, at most
    /// once a flight. One still
    /// expired after that renewal isn't handed out or renewed again: callers get a retryable
    /// failure, and the newest refresh token stays saved for the next try.
    fn fresh(&mut self, effects: &mut Vec<Effect>) {
        let expires_at = self
            .loaded
            .account
            .as_ref()
            .and_then(|account| account.expose().tokens.as_ref())
            .map(|tokens| tokens.expires_at);
        let now = self.now;
        let within = |seconds: u64| {
            expires_at.is_some_and(|expires_at| {
                now.is_none_or(|now| expires_at <= now.saturating_add(seconds))
            })
        };
        let refused = self.refused == Some(self.generation);
        if (refused || within(EARLY_SECONDS)) && !self.renewed && !self.tickets.is_empty() {
            return self.waiting.push_back(Waiting::Refresh(self.generation));
        }
        if within(0) {
            return self.fail(error_of(ErrorKey::SignInFailed, Action::Retry), effects);
        }
        self.granted(effects);
    }

    /// The pending-flag write for a refresh ended: mark the token as sent, unless its account
    /// was replaced.
    pub(super) fn refresh_started(
        &mut self,
        generation: u64,
        result: Result<(), VaultError>,
        effects: &mut Vec<Effect>,
    ) {
        self.lane = Lane::Free;
        let Some(marked) = self.marked(generation) else {
            // Nothing was sent.
            return self.jobs.push_back(Queued::Finished {
                outcome_unknown: false,
            });
        };
        if let Err(failure) = result {
            tracing::warn!(%failure, "the token lane's pending flag wasn't written");
            return self.fail(
                error_of(ErrorKey::CredentialsUnavailable, Action::Retry),
                effects,
            );
        }
        // Once sent, the token may be consumed, so the mark is durable before it goes out.
        self.lane = Lane::Refreshing {
            generation,
            client: None,
        };
        self.jobs.push_back(Queued::Rotate(
            Secret::new(marked),
            Rotation::Marked(generation),
        ));
    }

    /// The account for `generation` with its refresh token marked as sent, unless it was
    /// replaced.
    fn marked(&self, generation: u64) -> Option<Account> {
        let account = self
            .loaded
            .account
            .as_ref()
            .filter(|_| generation == self.generation)?;
        let mut account = account.expose().clone();
        account.tokens.as_mut()?.refresh_sent = true;
        Some(account)
    }

    /// The token's mark is saved: the refresh goes out, unless its account was replaced.
    fn send(&mut self, generation: u64, effects: &mut Vec<Effect>) {
        self.lane = Lane::Free;
        let Some(account) = self.marked(generation) else {
            // Nothing was sent.
            return self.jobs.push_back(Queued::Finished {
                outcome_unknown: false,
            });
        };
        let client = Secret::new(account.client_id.clone());
        let refresh_token = Secret::new(
            account
                .tokens
                .as_ref()
                .expect("a marked account has tokens")
                .refresh_token
                .clone(),
        );
        self.loaded.account = Some(Secret::new(account));
        self.renewed = true;
        effects.push(Effect::Refresh {
            generation,
            client: client.clone(),
            refresh_token,
        });
        self.lane = Lane::Refreshing {
            generation,
            client: Some(client),
        };
    }

    pub(super) fn refreshed(
        &mut self,
        generation: u64,
        result: Result<TokenSet, ExchangeError>,
        now: Option<u64>,
        effects: &mut Vec<Effect>,
    ) {
        if !matches!(&self.lane, Lane::Refreshing { generation: sent, client: Some(_) } if *sent == generation)
        {
            tracing::warn!("a refresh result with no refresh outstanding for it was ignored");
            return;
        }
        let Lane::Refreshing {
            client: Some(client),
            ..
        } = mem::replace(&mut self.lane, Lane::Free)
        else {
            return;
        };
        let current = generation == self.generation;
        self.now = now;
        let tokens = match result {
            Ok(tokens) => tokens,
            // A typed refusal issued nothing.
            Err(ExchangeError::OAuth { status, code }) => {
                self.jobs.push_back(Queued::Finished {
                    outcome_unknown: false,
                });
                if current {
                    self.refused(code.recovery(status), effects);
                }
                return;
            }
            // An unusable answer that named a successor: the token sent was consumed.
            Err(ExchangeError::Unusable {
                refresh_token: Some(token),
            }) => {
                if current {
                    self.end_session(effects);
                }
                return self.reject_successor(client.expose(), token.expose());
            }
            Err(failure) => {
                let (outcome_unknown, key) = classify(&failure);
                self.jobs.push_back(Queued::Finished { outcome_unknown });
                if !current {
                    return;
                }
                // The request may have reached the provider, so the token may be consumed.
                if outcome_unknown {
                    tracing::warn!(%failure, "a refresh's outcome is unknown; the session ends");
                    return self.session_lost(effects);
                }
                self.unmark();
                return self.fail(error_of(key, Action::Retry), effects);
            }
        };
        let successor = tokens.refresh_token.clone();
        let save = if current {
            self.rotated_set(tokens, now).map(|account| Save::Rotated {
                generation,
                account,
            })
        } else {
            // A sign-out replaced the account: the successor is its revocation entry.
            tokens
                .refresh_token
                .filter(|token| entry_fits(client.expose(), token.expose()))
                .map(|token| {
                    Save::List(Secret::new(Revocable {
                        client_id: client.expose().clone(),
                        refresh_token: token.expose().clone(),
                        added_at: 0,
                        origin: Origin::Account,
                    }))
                })
        };
        let Some(save) = save else {
            tracing::warn!("a refreshed set can't be saved");
            if !current {
                // A stale refresh that didn't rotate left the old token in place.
                return self.jobs.push_back(Queued::Finished {
                    outcome_unknown: successor.is_some(),
                });
            }
            if let Some(token) = successor {
                self.end_session(effects);
                return self.reject_successor(client.expose(), token.expose());
            }
            // The token sent wasn't replaced, so it may go again; the access token issued with
            // it has no owner.
            self.jobs.push_back(Queued::Finished {
                outcome_unknown: true,
            });
            self.unmark();
            return self.fail(error_of(ErrorKey::SignInFailed, Action::Retry), effects);
        };
        self.lane = Lane::Owning {
            attempt: self.attempt,
            save,
            held: None,
        };
        self.jobs.push_back(Queued::Save);
    }

    /// The account with `tokens` in place of its own, held for the identity check when an
    /// identity token came back; a refresh that leaves a token out keeps the old one.
    fn rotated_set(&self, tokens: TokenSet, now: Option<u64>) -> Option<Secret<Account>> {
        let mut account = self.loaded.account.as_ref()?.expose().clone();
        let old = account.tokens.take()?;
        let scope = tokens.scope.expose().clone();
        account.plan_enabled = permits_plan(&scope);
        account.tokens = Some(Tokens {
            access_token: tokens.access_token.expose().clone(),
            refresh_token: tokens
                .refresh_token
                .map_or(old.refresh_token, |token| token.expose().clone()),
            pending_identity_validation: tokens.id_token.is_some(),
            refresh_sent: false,
            connection: old.connection,
            id_token: tokens
                .id_token
                .map_or(old.id_token, |token| Some(token.expose().clone())),
            scope,
            // With no wall time, the set is due again at the next use.
            expires_at: now
                .and_then(|now| now.checked_add(tokens.expires_in))
                .unwrap_or(0),
        });
        account.validate().is_ok().then(|| Secret::new(account))
    }

    pub(super) fn rotated(&mut self, why: Rotation, result: SaveResult, effects: &mut Vec<Effect>) {
        match (why, result) {
            (Rotation::Marked(generation), SaveResult::Committed { .. }) => {
                self.send(generation, effects);
            }
            (Rotation::Marked(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "a refresh token wasn't marked as sent, so it wasn't sent");
                self.lane = Lane::Free;
                self.jobs.push_back(Queued::Finished {
                    outcome_unknown: false,
                });
                self.fail(
                    error_of(ErrorKey::CredentialsUnavailable, Action::Retry),
                    effects,
                );
            }
            (Rotation::Rotated(generation), SaveResult::Committed { .. }) => {
                let Lane::Owning {
                    save: Save::Rotated { account, .. },
                    ..
                } = mem::replace(&mut self.lane, Lane::Free)
                else {
                    debug_assert!(false, "a rotated save ended with no rotated set owned");
                    return;
                };
                if generation != self.generation || self.leaving.is_some() {
                    return;
                }
                // A new set is a new generation: a check or refresh still meant for the set it
                // replaced is stale.
                self.generation += 1;
                let pending = account
                    .expose()
                    .tokens
                    .as_ref()
                    .is_some_and(|tokens| tokens.pending_identity_validation);
                self.loaded.account = Some(account);
                if pending {
                    self.validate(effects);
                } else {
                    self.fresh(effects);
                }
            }
            (Rotation::Rotated(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "a rotated set is held for another save");
                self.hold(effects);
            }
            (Rotation::Verified(generation), SaveResult::Committed { .. }) => {
                self.validating = None;
                if generation != self.generation || self.leaving.is_some() {
                    return;
                }
                if let Some(account) = &self.loaded.account {
                    let mut account = account.expose().clone();
                    if let Some(tokens) = account.tokens.as_mut() {
                        tokens.pending_identity_validation = false;
                    }
                    self.loaded.account = Some(Secret::new(account));
                }
                self.fresh(effects);
            }
            (Rotation::Verified(_), SaveResult::NotCommitted(failure)) => {
                self.validating = None;
                tracing::warn!(%failure, "a verified set stays pending until it's saved");
                self.fail(
                    error_of(ErrorKey::CredentialsUnavailable, Action::Retry),
                    effects,
                );
            }
            (Rotation::Ended(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "an ended session's tokens stay on disk until the next refresh");
            }
            (Rotation::Unmarked(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "a refresh token stays marked as sent on disk; the next start ends the session");
            }
            (Rotation::Ended(_) | Rotation::Unmarked(_), SaveResult::Committed { .. }) => {}
        }
    }

    pub(super) fn validated(
        &mut self,
        generation: u64,
        result: Result<(), IdentityError>,
        now: Option<u64>,
        effects: &mut Vec<Effect>,
    ) {
        if self.validating != Some(generation) {
            return;
        }
        self.now = now;
        match result {
            // The check's flight lasts until its save ends, so Access joins that save too.
            Ok(()) => self.jobs.push_back(Queued::Verify(generation)),
            Err(IdentityError::OtherAccount | IdentityError::Token) => self.reject_set(effects),
            Err(failure) => {
                self.validating = None;
                tracing::warn!(%failure, "a rotated set's identity check failed; it stays pending");
                self.fail(error_of(identity_key(failure), Action::Retry), effects);
            }
        }
    }

    /// The check's save for `generation`, built from the account as it is now with only its
    /// verification added, so it can't undo a sent mark saved since; none once it was replaced.
    pub(super) fn verify_save(&self, generation: u64) -> Option<(Flight, Job)> {
        let account = self
            .loaded
            .account
            .as_ref()
            .filter(|_| generation == self.generation)?;
        let mut account = account.expose().clone();
        account.tokens.as_mut()?.pending_identity_validation = false;
        Some((
            Flight::Rotate(Rotation::Verified(generation)),
            Job::Rotate {
                account: Secret::new(account),
                finish: false,
            },
        ))
    }

    /// Checks the pending set's identity token against the saved subject, off the lane.
    fn validate(&mut self, effects: &mut Vec<Effect>) {
        let account = self.loaded.account.as_ref().map(Secret::expose);
        let parts = account.and_then(|account| {
            let tokens = account.tokens.as_ref()?;
            Some((
                account.client_id.clone(),
                account.subject.clone()?,
                tokens.id_token.clone()?,
            ))
        });
        // A pending set with no verified subject or no token to check can never pass.
        let Some((client, subject, id_token)) = parts else {
            return self.reject_set(effects);
        };
        self.validating = Some(self.generation);
        effects.push(Effect::Validate {
            generation: self.generation,
            client: Secret::new(client),
            subject: Secret::new(subject),
            id_token: Secret::new(id_token),
        });
    }

    fn refused(&mut self, recovery: Recovery, effects: &mut Vec<Effect>) {
        let failed = match recovery {
            Recovery::SignInAgain => {
                let ended = self.end_session(effects);
                return self.jobs.extend(ended);
            }
            Recovery::RetryLater => error_of(ErrorKey::AccountOffline, Action::Retry),
            Recovery::FixRequest => no_action(ErrorKey::AccountMisconfigured),
            Recovery::Unavailable => no_action(ErrorKey::PlanUnavailable),
        };
        // A refusal issued nothing, so the token may go again.
        self.unmark();
        self.fail(failed, effects);
    }

    /// Nothing was issued for the token sent: its mark is cleared, so it may go again.
    fn unmark(&mut self) {
        let Some(mut account) = self.loaded.account.as_ref().map(|a| a.expose().clone()) else {
            return;
        };
        let Some(tokens) = account.tokens.as_mut() else {
            return;
        };
        tokens.refresh_sent = false;
        let account = Secret::new(account);
        self.loaded.account = Some(account.clone());
        self.jobs
            .push_back(Queued::Rotate(account, Rotation::Unmarked(self.generation)));
    }

    /// The session ended: the tokens go, the registration stays, and the account is replaced.
    /// Returns the registration's save, which only a refused token's caller queues; any other token
    /// stays on disk until a sign-in's Commit or the sign-out's Prepare owns it.
    fn end_session(&mut self, effects: &mut Vec<Effect>) -> Option<Queued> {
        self.session_lost(effects);
        let registration = Secret::new(Account {
            plan_enabled: false,
            tokens: None,
            ..self.loaded.account.as_ref()?.expose().clone()
        });
        self.loaded.account = Some(registration.clone());
        Some(Queued::Rotate(
            registration,
            Rotation::Ended(self.generation),
        ))
    }

    /// The session can't go on: callers are told to sign in again and the account moves. Tokens
    /// left in place stay for sign-out to revoke.
    fn session_lost(&mut self, effects: &mut Vec<Effect>) {
        self.fail(error_of(ErrorKey::SessionEnded, Action::SignIn), effects);
        self.moved(effects);
        self.ended = Some(self.generation);
    }

    /// A refresh is out, or the lane still owns its result's save: shutdown waits for it, so a
    /// rotated set the provider issued isn't lost.
    #[must_use]
    pub fn refresh_out(&self) -> bool {
        matches!(
            self.lane,
            Lane::Refreshing { .. }
                | Lane::Owning {
                    save: Save::Rotated { .. } | Save::Discard(_),
                    held: None,
                    ..
                }
        )
    }

    pub(super) fn session_ended(&self) -> bool {
        self.ended == Some(self.generation)
    }

    /// A rotated set that can't be verified: the session ends, and the set stays in `chatgpt`,
    /// pending so it's never used, until a sign-in's Commit or the sign-out's Prepare owns it.
    fn reject_set(&mut self, effects: &mut Vec<Effect>) {
        self.end_session(effects);
    }

    /// A successor Kumochi can't keep goes to the list, as an exchange's set, once the lane is
    /// free, and its request ends then; with no entry or slot for it, its outcome is unknown. The
    /// token it replaced stays in `chatgpt`, marked as sent, so it's never sent again.
    fn reject_successor(&mut self, client: &Zeroizing<String>, token: &Zeroizing<String>) {
        let entry = entry_fits(client, token).then(|| {
            Secret::new(Revocable {
                client_id: client.clone(),
                refresh_token: token.clone(),
                added_at: 0,
                origin: Origin::Exchange,
            })
        });
        self.waiting.push_front(Waiting::Discard(entry));
    }

    /// The account's token set is replaced or removed: anything for the old one is stale, and
    /// its callers are told so.
    pub(super) fn moved(&mut self, effects: &mut Vec<Effect>) {
        self.generation += 1;
        self.validating = None;
        self.unavailable = false;
        self.answer(&Err(Denied::Unusable), effects);
    }

    fn granted(&mut self, effects: &mut Vec<Effect>) {
        let account = self.loaded.account.as_ref().map(Secret::expose);
        let result = match account
            .and_then(|account| Some((account.plan_enabled, account.tokens.as_ref()?)))
        {
            Some((true, tokens)) => Ok(Secret::new(tokens.access_token.clone())),
            Some((false, _)) => Err(Denied::Failed(error_of(
                ErrorKey::PlanNotEnabled,
                Action::Enable,
            ))),
            None => Err(Denied::Unusable),
        };
        self.answer(&result, effects);
    }

    pub(super) fn fail(&mut self, failed: UserError, effects: &mut Vec<Effect>) {
        self.answer(&Err(Denied::Failed(failed)), effects);
    }

    /// Ends the flight, answering every waiting caller alike.
    fn answer(
        &mut self,
        result: &Result<Secret<Zeroizing<String>>, Denied>,
        effects: &mut Vec<Effect>,
    ) {
        self.refreshing = None;
        self.renewed = false;
        for ticket in self.tickets.drain(..) {
            effects.push(Effect::Access {
                ticket,
                generation: self.generation,
                result: result.clone(),
            });
        }
    }
}

pub(super) fn error_of(key: ErrorKey, action: Action) -> UserError {
    UserError::new(key, Vec::new(), vec![action]).expect("one action is within the limit")
}

fn no_action(key: ErrorKey) -> UserError {
    UserError::new(key, Vec::new(), Vec::new()).expect("no action is within the limit")
}
