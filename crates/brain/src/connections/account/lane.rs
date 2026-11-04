//! The credential lane (design §3): the one token request it admits and the save that gives its
//! result a durable owner, the queue of credential jobs run one at a time, and how each finished
//! job moves the machine on.

use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ErrorKey};
use zeroize::Zeroizing;

use super::{AccountState, Done, Effect, Job, Live, Step, Why, error, exchanges};
use crate::connections::error::VaultError;
use crate::connections::marker::MarkerUpdate;
use crate::connections::oauth::{Authorization, Intent};
use crate::connections::records::{Account, MAX_EXCHANGES, Origin, Revocable};
use crate::connections::vault::SaveResult;

/// The one token request the lane admits, from its pending-flag write to its durable owner.
#[derive(Debug)]
pub(super) enum Lane {
    Free,
    /// The pending flag is being written; the code isn't sent yet.
    Starting {
        attempt: u64,
        authorization: Authorization,
    },
    /// The request is out.
    Outstanding {
        attempt: u64,
        client: Secret<Zeroizing<String>>,
    },
    /// The received set waits for its save, or holds after a refused one.
    Owning {
        attempt: u64,
        save: Save,
        /// The attempt current when its save was refused: the save is tried again once the
        /// attempt moves, or on Retry, never on every pass.
        held: Option<u64>,
    },
}

#[derive(Debug)]
pub(super) enum Save {
    Account(Secret<Account>),
    List(Secret<Revocable>),
}

/// Credential work waiting for the lane's thread.
#[derive(Debug)]
pub(super) enum Queued {
    Load {
        patient: bool,
    },
    Register {
        attempt: u64,
        account: Secret<Account>,
    },
    Started,
    Finished {
        outcome_unknown: bool,
    },
    /// The lane's save, built at dispatch so it goes to the owner current then.
    Save,
    Relist {
        confirmed: Vec<Secret<Revocable>>,
        why: Why,
    },
    /// A sign-out step.
    Leaving(Job),
}

/// The credential job in flight.
#[derive(Debug)]
pub(super) enum Flight {
    Load,
    Register {
        attempt: u64,
        account: Secret<Account>,
    },
    Started,
    Finished,
    Commit,
    List,
    Relist(Why),
    Leaving,
}

/// A sign-in op that arrived while the current attempt's save was running (R2-4): it waits for
/// that save, and only the newest is kept. A sign-out never waits here.
#[derive(Debug, Clone, Copy)]
pub(super) enum Deferred {
    SignIn(Intent),
    Cancel,
}

impl AccountState {
    pub(super) fn wait_for_lane(&mut self, authorization: Authorization) {
        if let Some(live) = &mut self.live {
            live.step = Step::Exchanging;
        }
        self.waiting.push_back((self.attempt, authorization));
    }

    pub(super) fn done(&mut self, done: Done, effects: &mut Vec<Effect>) {
        let flight = self.flight.take();
        match (flight, done) {
            (Some(Flight::Load), Done::Loaded(result)) => {
                self.loading = false;
                self.loaded(result, effects);
            }
            (Some(Flight::Register { attempt, account }), Done::Registered(result)) => {
                let registering = attempt == self.attempt
                    && matches!(
                        self.live,
                        Some(Live {
                            step: Step::Registering(_),
                            ..
                        })
                    );
                match result {
                    SaveResult::Committed { leftovers } => {
                        if leftovers {
                            tracing::warn!("old registration chunks remain until the next save");
                        }
                        self.loaded.account = Some(account);
                        if registering
                            && let Some(live) = &mut self.live
                            && let Step::Registering(authorization) =
                                mem::replace(&mut live.step, Step::Exchanging)
                        {
                            self.wait_for_lane(authorization);
                        }
                    }
                    SaveResult::NotCommitted(failure) => {
                        tracing::warn!(%failure, "the new registration wasn't saved");
                        if registering {
                            self.end_attempt(ErrorKey::CredentialsUnavailable, effects);
                        }
                    }
                }
            }
            (Some(Flight::Started), Done::Marked(result)) => self.started(result, effects),
            (Some(Flight::Finished), Done::Marked(result)) => {
                if let Err(failure) = result {
                    tracing::warn!(%failure, "the token lane's pending flag stays set");
                }
            }
            (
                Some(Flight::Commit),
                Done::Committed {
                    result,
                    notice_cleared,
                },
            ) => {
                // A sign-out admitted during the save keeps its pessimistic notice.
                if notice_cleared && self.leaving.is_none() {
                    self.loaded.notice = false;
                }
                self.committed(result, effects);
                // The save is published before a waiting op starts anything (R2-4).
                if self.deferred.is_some() {
                    self.publish(effects);
                }
                match self.deferred.take() {
                    Some(Deferred::SignIn(intent)) => self.sign_in(intent, effects),
                    Some(Deferred::Cancel) => self.cancel(effects),
                    None => {}
                }
            }
            (Some(Flight::List), Done::Listed { result, entries }) => {
                if !matches!(self.lane, Lane::Owning { .. }) {
                    debug_assert!(false, "a list save ended with no set owned");
                    return;
                }
                match result {
                    SaveResult::Committed { leftovers } => {
                        if leftovers {
                            tracing::warn!("old revocation chunks remain until the next sweep");
                        }
                        self.loaded.exchanges = exchanges(&entries);
                        self.lane = Lane::Free;
                        // An entry was added: revoke it now (design §2).
                        self.revoke(entries, effects);
                    }
                    SaveResult::NotCommitted(failure) => {
                        tracing::warn!(%failure, "a received token set is held for another save");
                        self.hold(effects);
                    }
                }
            }
            (Some(Flight::Relist(why)), Done::Relisted(result)) => {
                self.relisted(why, result, effects);
            }
            (Some(Flight::Leaving), done) => self.left(done, effects),
            (flight, done) => {
                debug_assert!(false, "{done:?} doesn't answer {flight:?}");
            }
        }
    }

    /// The pending flag's write ended: send the code, or end the attempt it was for.
    fn started(&mut self, result: Result<(), VaultError>, effects: &mut Vec<Effect>) {
        let Lane::Starting {
            attempt,
            authorization,
        } = mem::replace(&mut self.lane, Lane::Free)
        else {
            debug_assert!(false, "a pending flag was written with no request starting");
            return;
        };
        match result {
            Ok(()) if attempt == self.attempt => {
                self.lane = Lane::Outstanding {
                    attempt,
                    client: authorization.client_id.clone(),
                };
                effects.push(Effect::Exchange {
                    attempt,
                    authorization,
                });
            }
            // The attempt moved on before its code was sent: nothing was issued.
            Ok(()) => self.jobs.push_back(Queued::Finished {
                outcome_unknown: false,
            }),
            Err(failure) => {
                tracing::warn!(%failure, "the token lane's pending flag wasn't written");
                if attempt == self.attempt {
                    self.end_attempt(ErrorKey::CredentialsUnavailable, effects);
                }
            }
        }
    }

    fn committed(&mut self, result: SaveResult, effects: &mut Vec<Effect>) {
        let Lane::Owning {
            attempt,
            save: Save::Account(account),
            ..
        } = &mut self.lane
        else {
            debug_assert!(false, "a commit ended with no account save owned");
            return;
        };
        let attempt = *attempt;
        match result {
            SaveResult::Committed { leftovers } => {
                if leftovers {
                    tracing::warn!("old account chunks remain until the next save");
                }
                let plan = account.expose().plan_enabled;
                self.loaded.account = Some(account.clone());
                self.lane = Lane::Free;
                if attempt == self.attempt
                    && let Some(live) = self.live.take()
                    && live.intent == Intent::EnablePlan
                    && !plan
                {
                    effects.push(error(ErrorKey::PlanNotEnabled, Action::Enable));
                }
            }
            SaveResult::NotCommitted(failure) => {
                tracing::warn!(%failure, "a verified sign-in is held for another save");
                self.hold(effects);
            }
        }
    }

    /// The lane's save was refused: the set stays in memory until Retry, or the next attempt,
    /// saves it. A live attempt ends, since its code could only wait behind the held set.
    fn hold(&mut self, effects: &mut Vec<Effect>) {
        if let Lane::Owning { held, .. } = &mut self.lane {
            *held = Some(self.attempt);
        }
        if let Some(live) = self.live.take() {
            if matches!(live.step, Step::Listening) {
                effects.push(Effect::Close);
            }
            let current = self.attempt;
            self.waiting.retain(|(attempt, _)| *attempt != current);
        }
        effects.push(error(ErrorKey::CredentialsUnavailable, Action::Retry));
    }

    /// Moves work forward: a held set whose attempt moved on goes to the revocation list, the
    /// lane admits the next exchange, and the next credential job is dispatched.
    pub(super) fn pump(&mut self, effects: &mut Vec<Effect>) {
        if let Lane::Owning { held, .. } = &mut self.lane
            && held.is_some_and(|at| at != self.attempt)
        {
            *held = None;
            self.jobs.push_back(Queued::Save);
        }
        while matches!(self.lane, Lane::Free) {
            let Some((attempt, authorization)) = self.waiting.pop_front() else {
                break;
            };
            // A replaced attempt's code is never sent.
            if attempt != self.attempt {
                continue;
            }
            if self.loaded.exchanges >= MAX_EXCHANGES {
                self.end_attempt(ErrorKey::RevocationBacklog, effects);
                continue;
            }
            self.lane = Lane::Starting {
                attempt,
                authorization,
            };
            self.jobs.push_back(Queued::Started);
        }
        self.settle();
        self.dispatch(effects);
    }

    pub(super) fn dispatch(&mut self, effects: &mut Vec<Effect>) {
        while self.flight.is_none() {
            let Some(queued) = self.jobs.pop_front() else {
                return;
            };
            let (flight, job) = match queued {
                Queued::Load { patient } => (Flight::Load, Job::Load { patient }),
                // A replaced attempt's registration isn't saved.
                Queued::Register { attempt, .. } if attempt != self.attempt => continue,
                Queued::Register { attempt, account } => (
                    Flight::Register {
                        attempt,
                        account: account.clone(),
                    },
                    Job::Register(account),
                ),
                Queued::Started => (Flight::Started, Job::Marker(MarkerUpdate::RequestStarted)),
                Queued::Finished { outcome_unknown } => (
                    Flight::Finished,
                    Job::Marker(MarkerUpdate::RequestFinished { outcome_unknown }),
                ),
                Queued::Save => {
                    let current = self.attempt;
                    let Lane::Owning { attempt, save, .. } = &mut self.lane else {
                        debug_assert!(false, "a save was queued with no set owned");
                        continue;
                    };
                    // Revalidated just before dispatch: a stale attempt's set is revoked instead.
                    if *attempt != current
                        && let Save::Account(account) = save
                    {
                        let account = account.expose();
                        let tokens = account.tokens.as_ref().expect("a sign-in save has tokens");
                        *save = listed(account.client_id.clone(), tokens.refresh_token.clone());
                    }
                    match save {
                        Save::Account(account) => (Flight::Commit, Job::Commit(account.clone())),
                        Save::List(entry) => (Flight::List, Job::List(entry.clone())),
                    }
                }
                Queued::Relist { confirmed, why } => (
                    Flight::Relist(why),
                    Job::Relist {
                        confirmed,
                        age_out: why == Why::Admit,
                    },
                ),
                Queued::Leaving(job) => (Flight::Leaving, job),
            };
            self.flight = Some(flight);
            effects.push(Effect::Vault(job));
        }
    }

    /// The current attempt's save is running, so ops wait for it (R2-4).
    pub(super) fn saving_current(&self) -> bool {
        matches!(self.flight, Some(Flight::Commit)) && self.lane_attempt() == Some(self.attempt)
    }

    pub(super) fn held(&self) -> bool {
        matches!(self.lane, Lane::Owning { held: Some(_), .. })
    }

    pub(super) fn lane_attempt(&self) -> Option<u64> {
        match &self.lane {
            Lane::Free => None,
            Lane::Starting { attempt, .. }
            | Lane::Outstanding { attempt, .. }
            | Lane::Owning { attempt, .. } => Some(*attempt),
        }
    }
}

pub(super) fn listed(client_id: Zeroizing<String>, refresh_token: Zeroizing<String>) -> Save {
    Save::List(Secret::new(Revocable {
        client_id,
        refresh_token,
        added_at: 0,
        origin: Origin::Exchange,
    }))
}
