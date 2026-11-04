//! The credential lane (design §3): the one token request it admits and the save that gives its
//! result a durable owner, the queue of credential jobs run one at a time, and how each finished
//! job moves the machine on.

use std::mem;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ErrorKey};
use zeroize::Zeroizing;

use super::refresh::{Rotation, error_of};
use super::{AccountState, Done, Effect, Job, Live, Step, Why, error, exchanges};
use crate::connections::error::VaultError;
use crate::connections::marker::MarkerUpdate;
use crate::connections::oauth::{Authorization, Intent};
use crate::connections::records::{Account, MAX_EXCHANGES, Origin, Revocable};
use crate::connections::vault::SaveResult;

/// The marker write for a token whose outcome is unknown.
const UNKNOWN: MarkerUpdate = MarkerUpdate::RequestFinished {
    outcome_unknown: true,
};

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
    /// A refresh for `generation`: its pending flag is being written, then, with its client,
    /// it's out.
    Refreshing {
        generation: u64,
        client: Option<Secret<Zeroizing<String>>>,
    },
}

#[derive(Debug)]
pub(super) enum Save {
    Account(Secret<Account>),
    List(Secret<Revocable>),
    /// A refresh token for the list that no request on the lane owns: its save finishes no
    /// request, so it can't clear another's pending flag.
    Discard(Secret<Revocable>),
    /// A refresh's rotated set, for the generation it serves.
    Rotated {
        generation: u64,
        account: Secret<Account>,
    },
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
    /// A save of the account outside the lane, for a refresh flight.
    Rotate(Secret<Account>, Rotation),
    /// A check's save, built at dispatch from the account as it is then.
    Verify(u64),
    /// The unknown outcome of a token no list slot holds; the jobs after it run only once it's
    /// written, so the token never leaves the account unrecorded.
    Unknown(Vec<Queued>),
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
    Rotate(Rotation),
    Unknown(Vec<Queued>),
}

/// A sign-in op that arrived while the current attempt's save was running (R2-4): it waits for
/// that save, and only the newest is kept. A sign-out never waits here.
#[derive(Debug, Clone, Copy)]
pub(super) enum Deferred {
    SignIn(Intent),
    Cancel,
}

/// A token request waiting for the lane, in arrival order.
#[derive(Debug)]
pub(super) enum Waiting {
    /// An attempt's code.
    Exchange(u64, Authorization),
    /// The refresh for a generation.
    Refresh(u64),
    /// A refresh token for the list, none when no entry could hold it, and the jobs that run
    /// only once it's listed or counted unknown.
    Discard {
        entry: Option<Secret<Revocable>>,
        then: Vec<Queued>,
    },
}

impl AccountState {
    pub(super) fn wait_for_lane(&mut self, authorization: Authorization) {
        if let Some(live) = &mut self.live {
            live.step = Step::Exchanging;
        }
        self.waiting
            .push_back(Waiting::Exchange(self.attempt, authorization));
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
            (Some(Flight::Unknown(then)), Done::Marked(result)) => {
                self.unknown_written(then, result);
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
                        self.jobs.extend(mem::take(&mut self.after_list));
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
            (Some(Flight::Rotate(why)), Done::Rotated(result, now)) => {
                self.now = now;
                self.rotated(why, result, effects);
            }
            (Some(Flight::Leaving), done) => self.left(done, effects),
            (flight, done) => {
                debug_assert!(false, "{done:?} doesn't answer {flight:?}");
            }
        }
    }

    /// The pending flag's write ended: send the code, or end the attempt it was for.
    fn started(&mut self, result: Result<(), VaultError>, effects: &mut Vec<Effect>) {
        if let Lane::Refreshing { generation, .. } = self.lane {
            return self.refresh_started(generation, result, effects);
        }
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
                self.moved(effects);
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
    pub(super) fn hold(&mut self, effects: &mut Vec<Effect>) {
        if let Lane::Owning { held, .. } = &mut self.lane {
            *held = Some(self.attempt);
        }
        if let Some(live) = self.live.take() {
            if matches!(live.step, Step::Listening) {
                effects.push(Effect::Close);
            }
            let current = self.attempt;
            self.waiting.retain(
                |waiting| !matches!(waiting, Waiting::Exchange(attempt, _) if *attempt == current),
            );
        }
        effects.push(error(ErrorKey::CredentialsUnavailable, Action::Retry));
        // A refresh waiting behind the held set can't go out, so its callers are told now.
        self.waiting
            .retain(|waiting| !matches!(waiting, Waiting::Refresh(_)));
        self.fail(
            error_of(ErrorKey::CredentialsUnavailable, Action::Retry),
            effects,
        );
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
            let (attempt, authorization) = match self.waiting.pop_front() {
                None => break,
                Some(Waiting::Exchange(attempt, authorization)) => (attempt, authorization),
                Some(Waiting::Discard { entry, then }) => {
                    let Some(entry) = entry.filter(|_| self.loaded.exchanges < MAX_EXCHANGES)
                    else {
                        tracing::warn!(
                            "a discarded set no list slot can hold has an unknown outcome"
                        );
                        self.jobs.push_back(Queued::Unknown(then));
                        continue;
                    };
                    self.after_list = then;
                    self.lane = Lane::Owning {
                        attempt: self.attempt,
                        save: Save::Discard(entry),
                        held: None,
                    };
                    self.jobs.push_back(Queued::Save);
                    continue;
                }
                // A replaced account's refresh is never sent.
                Some(Waiting::Refresh(generation)) => {
                    if generation == self.generation {
                        self.lane = Lane::Refreshing {
                            generation,
                            client: None,
                        };
                        self.jobs.push_back(Queued::Started);
                    }
                    continue;
                }
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
                Queued::Unknown(then) => (Flight::Unknown(then), Job::Marker(UNKNOWN)),
                // A save for a replaced account never overwrites its successor.
                Queued::Rotate(_, Rotation::Ended(generation) | Rotation::Unmarked(generation))
                    if generation != self.generation =>
                {
                    continue;
                }
                Queued::Verify(generation) => match self.verify_save(generation) {
                    Some(flight) => flight,
                    None => continue,
                },
                Queued::Rotate(account, why) => (
                    Flight::Rotate(why),
                    Job::Rotate {
                        account,
                        finish: false,
                    },
                ),
                Queued::Save => {
                    let (current, generation_now) = (self.attempt, self.generation);
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
                    // A replaced account's rotated set is its successor: the `Account` entry.
                    if let Save::Rotated {
                        generation,
                        account,
                    } = save
                        && *generation != generation_now
                    {
                        let account = account.expose();
                        let tokens = account.tokens.as_ref().expect("a rotated set has tokens");
                        *save = Save::List(Secret::new(Revocable {
                            client_id: account.client_id.clone(),
                            refresh_token: tokens.refresh_token.clone(),
                            added_at: 0,
                            origin: Origin::Account,
                        }));
                    }
                    let finish = matches!(save, Save::List(_));
                    match save {
                        Save::Account(account) => (Flight::Commit, Job::Commit(account.clone())),
                        Save::List(entry) | Save::Discard(entry) => (
                            Flight::List,
                            Job::List {
                                entry: entry.clone(),
                                finish,
                            },
                        ),
                        Save::Rotated {
                            generation,
                            account,
                        } => (
                            Flight::Rotate(Rotation::Rotated(*generation)),
                            Job::Rotate {
                                account: account.clone(),
                                finish: true,
                            },
                        ),
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

    /// A token's unknown outcome was written, so the jobs after it may run; if it wasn't, the
    /// token stays in the account, still owned, for a later pass to settle.
    fn unknown_written(&mut self, then: Vec<Queued>, result: Result<(), VaultError>) {
        match result {
            Ok(()) => self.jobs.extend(then),
            Err(failure) => {
                tracing::warn!(%failure, "an unrecorded token's outcome wasn't written; the account keeps it");
            }
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
            Lane::Free | Lane::Refreshing { .. } => None,
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
