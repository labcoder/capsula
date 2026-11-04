//! Refresh (design §3 rule 3). A caller asks for the bearer; a set within a minute of expiry is
//! refreshed first, on the token lane. The rotated set is saved before anything else goes out,
//! then its identity token is checked against the saved subject. Callers join that one flight
//! until the set is verified. A result for a replaced account never becomes the account.

use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ErrorKey, UserError};
use zeroize::Zeroizing;

use super::lane::{Lane, Queued, Save, Waiting};
use super::{AccountState, Denied, Effect, classify, identity_key};
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
}

impl AccountState {
    pub(super) fn access(&mut self, ticket: u64, now: Option<u64>, effects: &mut Vec<Effect>) {
        let tokens = match &self.loaded.account {
            Some(account) if !self.busy && self.leaving.is_none() => {
                account.expose().tokens.as_ref()
            }
            _ => None,
        };
        let Some((pending, expires_at)) =
            tokens.map(|tokens| (tokens.pending_identity_validation, tokens.expires_at))
        else {
            return effects.push(Effect::Access {
                ticket,
                result: Err(Denied::Unusable),
            });
        };
        self.tickets.push(ticket);
        if self.refreshing.is_some() {
            return;
        }
        self.refreshing = Some(self.generation);
        // A held save goes again first; a held rotated set is never requested again, and its
        // commit answers.
        if let Lane::Owning {
            save,
            held: held @ Some(_),
            ..
        } = &mut self.lane
        {
            *held = None;
            self.jobs.push_back(Queued::Save);
            if matches!(save, Save::Rotated { .. }) {
                return;
            }
        }
        if pending {
            return self.validate(effects);
        }
        if now.is_none_or(|now| expires_at <= now.saturating_add(EARLY_SECONDS)) {
            return self.waiting.push_back(Waiting::Refresh(self.generation));
        }
        self.granted(effects);
    }

    /// The pending-flag write for a refresh ended: send it, unless its account was replaced.
    pub(super) fn refresh_started(
        &mut self,
        generation: u64,
        result: Result<(), VaultError>,
        effects: &mut Vec<Effect>,
    ) {
        self.lane = Lane::Free;
        let account = self
            .loaded
            .account
            .as_ref()
            .filter(|_| generation == self.generation)
            .map(Secret::expose);
        let Some((client, tokens)) =
            account.and_then(|account| Some((&account.client_id, account.tokens.as_ref()?)))
        else {
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
        let client = Secret::new(client.clone());
        effects.push(Effect::Refresh {
            generation,
            client: client.clone(),
            refresh_token: Secret::new(tokens.refresh_token.clone()),
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
            // An unusable answer that named a successor: the list owns it, as an exchange's.
            Err(ExchangeError::Unusable {
                refresh_token: Some(token),
            }) => {
                if current {
                    self.fail(error_of(ErrorKey::SignInFailed, Action::Retry), effects);
                }
                return self.discard(client.expose(), token.expose());
            }
            Err(failure) => {
                let (outcome_unknown, key) = classify(&failure);
                if outcome_unknown {
                    tracing::warn!(%failure, "a refresh's outcome is unknown");
                }
                self.jobs.push_back(Queued::Finished { outcome_unknown });
                if current {
                    self.fail(error_of(key, Action::Retry), effects);
                }
                return;
            }
        };
        let rotated = tokens.refresh_token.is_some();
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
            // A stale refresh that didn't rotate left the old token in place.
            self.jobs.push_back(Queued::Finished {
                outcome_unknown: current || rotated,
            });
            if current {
                self.fail(error_of(ErrorKey::SignInFailed, Action::Retry), effects);
            }
            return;
        };
        self.lane = Lane::Owning {
            attempt: self.attempt,
            save,
            held: None,
        };
        self.jobs.push_back(Queued::Save);
    }

    /// The account with \`tokens\` in place of its own, held for the identity check when an
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
                let pending = account
                    .expose()
                    .tokens
                    .as_ref()
                    .is_some_and(|tokens| tokens.pending_identity_validation);
                self.loaded.account = Some(account);
                if pending {
                    self.validate(effects);
                } else {
                    self.granted(effects);
                }
            }
            (Rotation::Rotated(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "a rotated set is held for another save");
                self.hold(effects);
            }
            (Rotation::Verified(generation), SaveResult::Committed { .. }) => {
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
                self.granted(effects);
            }
            (Rotation::Verified(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "a verified set stays pending until it's saved");
                self.fail(
                    error_of(ErrorKey::CredentialsUnavailable, Action::Retry),
                    effects,
                );
            }
            (Rotation::Ended(_), SaveResult::NotCommitted(failure)) => {
                tracing::warn!(%failure, "an ended session's tokens stay on disk until the next refresh");
            }
            (Rotation::Ended(_), SaveResult::Committed { .. }) => {}
        }
    }

    pub(super) fn validated(
        &mut self,
        generation: u64,
        result: Result<(), IdentityError>,
        effects: &mut Vec<Effect>,
    ) {
        if self.validating != Some(generation) {
            return;
        }
        self.validating = None;
        match result {
            Ok(()) => {
                let Some(mut account) = self.loaded.account.as_ref().map(|a| a.expose().clone())
                else {
                    return;
                };
                if let Some(tokens) = account.tokens.as_mut() {
                    tokens.pending_identity_validation = false;
                }
                self.jobs.push_back(Queued::Rotate(
                    Secret::new(account),
                    Rotation::Verified(generation),
                ));
            }
            Err(IdentityError::OtherAccount | IdentityError::Token) => self.reject_set(effects),
            Err(failure) => {
                tracing::warn!(%failure, "a rotated set's identity check failed; it stays pending");
                self.fail(error_of(identity_key(failure), Action::Retry), effects);
            }
        }
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
            Recovery::SignInAgain => return self.end_session(effects),
            Recovery::RetryLater => error_of(ErrorKey::AccountOffline, Action::Retry),
            Recovery::FixRequest => no_action(ErrorKey::AccountMisconfigured),
            Recovery::Unavailable => no_action(ErrorKey::PlanUnavailable),
        };
        self.fail(failed, effects);
    }

    /// The session ended: the tokens go, the registration stays, and the account is replaced.
    fn end_session(&mut self, effects: &mut Vec<Effect>) {
        self.fail(error_of(ErrorKey::SessionEnded, Action::SignIn), effects);
        self.moved(effects);
        let Some(account) = &self.loaded.account else {
            return;
        };
        let account = account.expose();
        let registration = Secret::new(Account {
            plan_enabled: false,
            tokens: None,
            ..account.clone()
        });
        self.loaded.account = Some(registration.clone());
        self.ended = Some(self.generation);
        self.jobs.push_back(Queued::Rotate(
            registration,
            Rotation::Ended(self.generation),
        ));
    }

    pub(super) fn session_ended(&self) -> bool {
        self.ended == Some(self.generation)
    }

    /// A rotated set that can't be verified: its token goes to the list, and the session ends.
    fn reject_set(&mut self, effects: &mut Vec<Effect>) {
        let account = self.loaded.account.as_ref().map(Secret::expose);
        if let Some((client, token)) = account.and_then(|account| {
            let tokens = account.tokens.as_ref()?;
            Some((account.client_id.clone(), tokens.refresh_token.clone()))
        }) {
            self.discard(&client, &token);
        }
        self.end_session(effects);
    }

    /// Hands a refresh token to the revocation list once the lane is free, as an exchange's set.
    fn discard(&mut self, client: &Zeroizing<String>, token: &Zeroizing<String>) {
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
        for ticket in self.tickets.drain(..) {
            effects.push(Effect::Access {
                ticket,
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
