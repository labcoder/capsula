//! Credential work the account state machine asks for, run one job at a time on the
//! credential lane's thread, which owns the vault and its session lock.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use crate::connections::clock::Clock;
use crate::connections::error::VaultError;
use crate::connections::marker::{MarkerUpdate, SignOutMarker};
use crate::connections::records::{
    Account, MAX_EXCHANGES, Origin, Revocable, RevocationList, entry_fits,
};
use crate::connections::vault::{DeleteResult, SaveResult, Vault};
use crate::platform::credential::{CredentialStore, NativeCredentialStore};

/// How often a patient load tries the session lock [guess]: Windows releases a dead brain's
/// lock a little after the process ends.
const BUSY_TRIES: u32 = 5;
/// The wait between those tries [guess].
const BUSY_WAIT: Duration = Duration::from_millis(250);
/// An entry ages out 30 days after it was added, the documented refresh-token lifetime
/// (developers.openai.com/siwc/token-sharing-open-source/token-reference), plus a local 2-day
/// margin for clock skew.
const AGE_OUT_SECONDS: u64 = 32 * 86_400;
/// Wall time running this much further than monotonic time is a clock jump, and suspends
/// age-out until the next start (local policy).
const CLOCK_JUMP_SECONDS: u64 = 3_600;

/// One unit of credential work, executed whole before the next starts.
pub enum Job {
    /// Take the session lock if it isn't held yet, then read the account's state. A patient
    /// load retries an early Busy.
    Load {
        /// Retry Busy a few times first, as at startup.
        patient: bool,
    },
    /// Save a new registration before its exchange, so the issued client survives a failure.
    Register(Secret<Account>),
    /// Merge one lane fact into the marker.
    Marker(MarkerUpdate),
    /// Save a verified sign-in, once the token it replaces has a durable owner; once committed,
    /// release its lane fact and an old notice.
    Commit(Secret<Account>),
    /// Add a refresh token to the revocation list. Its `added_at` is stamped when it's saved.
    List {
        /// The entry.
        entry: Secret<Revocable>,
        /// Whether this save ends the lane's request, releasing its fact once committed.
        finish: bool,
    },
    /// Drop these confirmed entries, and with `age_out` the expired ones, from the revocation
    /// list. A list left empty has its names deleted and checked, then its sweep fact cleared.
    Relist {
        /// Entries an empty 200 revoked.
        confirmed: Vec<Secret<Revocable>>,
        /// Whether to drop entries past their age-out too.
        age_out: bool,
    },
    /// Sign-out's Prepare: adopt a token still in `chatgpt` as the `Account` entry unless one is
    /// listed, and persist uncertainty for a record that can't be read.
    Prepare,
    /// Sign-out's checkpoint: drop these confirmed entries, then decide the notice for good.
    Checkpoint(Vec<Secret<Revocable>>),
    /// Sign-out's Delete, then Finish.
    Finish,
    /// Save a refresh flight's account: its rotated set, the set once verified, or the
    /// registration left when the session ended. Only the lane's save, `finish`, releases the
    /// lane fact once committed.
    Rotate {
        /// What to save.
        account: Secret<Account>,
        /// Whether this save ends the lane's request.
        finish: bool,
    },
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Load { .. } => "Load",
            Self::Register(_) => "Register",
            Self::Marker(_) => "Marker",
            Self::Commit(_) => "Commit",
            Self::List { .. } => "List",
            Self::Relist { .. } => "Relist",
            Self::Prepare => "Prepare",
            Self::Checkpoint(_) => "Checkpoint",
            Self::Finish => "Finish",
            Self::Rotate { .. } => "Rotate",
        })
    }
}

/// How a [`Job`] ended, in the same order the jobs ran.
#[derive(Debug)]
pub enum Done {
    /// The account's state, or why it couldn't be read: Busy, or the lock failed.
    Loaded(Result<Box<Snapshot>, VaultError>),
    /// The registration's save.
    Registered(SaveResult),
    /// The marker write.
    Marked(Result<(), VaultError>),
    /// The sign-in's save, and whether an old sign-out notice was then cleared on disk.
    Committed {
        /// The save.
        result: SaveResult,
        /// The marker no longer carries the notice; only then is it cleared in memory.
        notice_cleared: bool,
        /// The list's entries once the replaced token was listed, whatever the save did.
        listed: Option<Vec<Secret<Revocable>>>,
    },
    /// The revocation list's save, and the entries it holds once committed.
    Listed {
        /// The save.
        result: SaveResult,
        /// The saved list's entries.
        entries: Vec<Secret<Revocable>>,
    },
    /// The entries still listed after a [`Job::Relist`], or why it stopped.
    Relisted(Result<Vec<Secret<Revocable>>, VaultError>),
    /// What a sign-out revokes, or why Prepare stopped.
    Prepared(Result<Vec<Secret<Revocable>>, VaultError>),
    /// The notice the checkpoint decided.
    Checkpointed(Result<bool, VaultError>),
    /// Delete and Finish ended.
    SignedOut(Result<(), VaultError>),
    /// A refresh flight's save, and the wall time it ended, for the bearer's expiry.
    Rotated(SaveResult, Option<u64>),
}

/// What the vault held when it was read. Each record fails on its own, so one damaged record
/// doesn't hide the others.
#[derive(Debug)]
pub struct Snapshot {
    /// Control state. Damaged data already reads as the fail-closed marker, so an error is a
    /// refused read: it decides nothing, and the other records aren't read (`Fenced`).
    pub marker: Result<SignOutMarker, VaultError>,
    /// This machine's host id, created on first use.
    pub host: Result<Secret<Zeroizing<String>>, VaultError>,
    /// The account, never usable while a sign-out is pending.
    pub account: Result<Option<Secret<Account>>, VaultError>,
    /// The revocation list's entries, after age-out unless a sign-out is pending.
    pub revocable: Result<Vec<Secret<Revocable>>, VaultError>,
}

/// Runs jobs against the vault it opens on the first load, and keeps it, with its session
/// lock, until dropped.
pub struct Worker<S: CredentialStore> {
    vault: Option<Vault<S>>,
    open: Box<dyn FnMut() -> Result<Vault<S>, VaultError> + Send>,
    clock: Arc<dyn Clock>,
    /// Wall and monotonic time at creation, to notice a clock jump.
    started: (Option<u64>, Instant),
    /// A clock jump was seen: nothing ages out until the next start.
    jumped: bool,
    /// Whether this worker holds the vault, and so the session lock: set once it opens, and
    /// cleared before it drops. Claude Code's lane reads it before taking its own lock.
    held: Arc<AtomicBool>,
}

impl<S: CredentialStore> Drop for Worker<S> {
    fn drop(&mut self) {
        // Cleared first: the vault, and with it the lock, drops after this.
        self.held.store(false, Ordering::Release);
    }
}

impl<S: CredentialStore> fmt::Debug for Worker<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Worker(***)")
    }
}

impl Worker<NativeCredentialStore> {
    /// The worker on the real credential store: the only way the brain reaches it outside tests.
    pub fn system(clock: Arc<dyn Clock>) -> Self {
        Self::new(Box::new(|| Vault::open(NativeCredentialStore)), clock)
    }
}

impl<S: CredentialStore> Worker<S> {
    pub(crate) fn anthropic(
        &mut self,
        op: crate::connections::anthropic::KeyOp,
    ) -> Result<
        Option<Secret<crate::connections::anthropic::key::AnthropicKey>>,
        crate::connections::anthropic::KeyError,
    > {
        self.vault.as_mut().ok_or(VaultError::Busy)?.anthropic(op)
    }

    /// A worker that opens its vault with `open` and stamps revocation entries with `clock`.
    pub fn new(
        open: Box<dyn FnMut() -> Result<Vault<S>, VaultError> + Send>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            vault: None,
            open,
            started: (clock.unix_seconds(), clock.monotonic()),
            clock,
            jumped: false,
            held: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The fact that this worker holds the session lock, for Claude Code's lane to read.
    pub fn held(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.held)
    }

    /// Runs one job to its end. Blocking: call it off the core loop.
    pub fn run(&mut self, job: Job) -> Done {
        if let Job::Load { patient } = job {
            let tries = if patient { BUSY_TRIES } else { 1 };
            return Done::Loaded(self.load(tries).map(Box::new));
        }
        let aged = matches!(job, Job::Relist { age_out: true, .. })
            .then(|| self.now_for_age_out())
            .flatten();
        // Never before this process's own time, so a clock set back then corrected can't age an
        // entry out early (CAPS-130). shortcut: with no wall time an entry never ages out; only a
        // confirmed revocation or a sign-out removes it.
        let now = self.clock.unix_seconds();
        let added_at = now
            .max(self.started.0.map(|wall| wall.saturating_add(self.ran())))
            .unwrap_or(u64::MAX);
        let Some(vault) = self.vault.as_mut() else {
            let refused = || SaveResult::NotCommitted(VaultError::Busy);
            return match job {
                Job::Load { .. } => unreachable!("handled above"),
                Job::Register(_) => Done::Registered(refused()),
                Job::Marker(_) => Done::Marked(Err(VaultError::Busy)),
                Job::Commit(_) => Done::Committed {
                    result: refused(),
                    notice_cleared: false,
                    listed: None,
                },
                Job::List { .. } => Done::Listed {
                    result: refused(),
                    entries: Vec::new(),
                },
                Job::Relist { .. } => Done::Relisted(Err(VaultError::Busy)),
                Job::Prepare => Done::Prepared(Err(VaultError::Busy)),
                Job::Checkpoint(_) => Done::Checkpointed(Err(VaultError::Busy)),
                Job::Finish => Done::SignedOut(Err(VaultError::Busy)),
                Job::Rotate { .. } => Done::Rotated(refused(), now),
            };
        };
        match job {
            Job::Load { .. } => unreachable!("handled above"),
            Job::Register(account) => Done::Registered(vault.save_account(&account)),
            Job::Marker(update) => Done::Marked(vault.update_marker(update).map(drop)),
            Job::Commit(account) => commit(vault, &account, added_at),
            Job::Rotate { account, finish } => {
                let result = rotate(vault, &account, finish);
                // A bearer's expiry is judged when its save ends, which can be long after it began.
                Done::Rotated(result, self.clock.unix_seconds())
            }
            Job::List { entry, finish } => {
                let list = vault.load_revocations().map(|list| {
                    let mut list =
                        list.map_or_else(RevocationList::default, |list| list.expose().clone());
                    let mut entry = entry.expose().clone();
                    // A refresh's successor replaces the account entry it rotated.
                    if entry.origin == Origin::Account {
                        list.entries
                            .retain(|listed| listed.origin != Origin::Account);
                    }
                    entry.added_at = added_at;
                    list.entries.push(entry);
                    list
                });
                let list = match list {
                    Ok(list) => list,
                    Err(error) => {
                        return Done::Listed {
                            result: SaveResult::NotCommitted(error),
                            entries: Vec::new(),
                        };
                    }
                };
                let result = vault.save_revocations(&Secret::new(list.clone()));
                if finish && matches!(result, SaveResult::Committed) {
                    finish_request(vault);
                }
                Done::Listed {
                    result,
                    entries: list.entries.into_iter().map(Secret::new).collect(),
                }
            }
            Job::Relist { confirmed, .. } => Done::Relisted(relist(vault, |entry| {
                confirmed.iter().any(|done| done.expose() == entry)
                    || aged.is_some_and(|now| expired(entry, now))
            })),
            Job::Prepare => Done::Prepared(prepare(vault, added_at)),
            Job::Checkpoint(confirmed) => Done::Checkpointed(checkpoint(vault, &confirmed)),
            Job::Finish => Done::SignedOut(match vault.delete_for_sign_out() {
                DeleteResult::Gone => vault.update_marker(MarkerUpdate::FinishSignOut).map(drop),
                DeleteResult::Refused(error) => Err(error),
            }),
        }
    }

    /// Wall time for age-out, or `None` while it's suspended: the clock is unreadable, or it
    /// jumped forward since this worker started.
    fn now_for_age_out(&mut self) -> Option<u64> {
        let now = self.clock.unix_seconds()?;
        let ran = self.ran();
        if self
            .started
            .0
            .is_none_or(|wall| now.saturating_sub(wall) > ran.saturating_add(CLOCK_JUMP_SECONDS))
        {
            if !self.jumped {
                tracing::warn!("the wall clock jumped; age-out waits for the next start");
            }
            self.jumped = true;
        }
        (!self.jumped).then_some(now)
    }

    /// Monotonic seconds since this worker started.
    fn ran(&self) -> u64 {
        self.clock
            .monotonic()
            .saturating_duration_since(self.started.1)
            .as_secs()
    }

    fn load(&mut self, mut tries: u32) -> Result<Snapshot, VaultError> {
        let opening = self.vault.is_none();
        if opening {
            let vault = loop {
                tries -= 1;
                match (self.open)() {
                    Err(VaultError::Busy) if tries > 0 => std::thread::sleep(BUSY_WAIT),
                    opened => break opened?,
                }
            };
            self.vault = Some(vault);
            self.held.store(true, Ordering::Release);
        }
        let aged = self.now_for_age_out();
        let vault = self.vault.as_mut().expect("opened above");
        let marker = match vault.marker() {
            Ok(marker) => marker,
            Err(error) => {
                return Ok(Snapshot {
                    marker: Err(error),
                    host: Err(VaultError::Fenced),
                    account: Err(VaultError::Fenced),
                    revocable: Err(VaultError::Fenced),
                });
            }
        };
        // A request the last run left pending may have issued a token nobody owns: that's
        // uncertainty now, before any later save can finish the flag as if it were known.
        let marker = if opening && marker.request_pending {
            vault
                .update_marker(MarkerUpdate::RequestFinished {
                    outcome_unknown: true,
                })
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "the last run's pending flag stays as it is");
                    marker
                })
        } else {
            marker
        };
        // Nothing leaves the list but a confirmed revocation while a sign-out is pending.
        let revocable = if marker.sign_out.is_some() {
            vault.load_revocations().map(|list| {
                list.map_or_else(Vec::new, |list| {
                    list.expose()
                        .entries
                        .iter()
                        .cloned()
                        .map(Secret::new)
                        .collect()
                })
            })
        } else {
            relist(vault, |entry| aged.is_some_and(|now| expired(entry, now)))
        };
        Ok(Snapshot {
            marker: Ok(marker),
            host: vault.host_id(),
            account: vault.load_account(),
            revocable,
        })
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Job::Rotate's own field, passed through"
)]
fn rotate<S: CredentialStore>(
    vault: &mut Vault<S>,
    account: &Secret<Account>,
    finish: bool,
) -> SaveResult {
    let result = vault.save_account(account);
    if finish && matches!(result, SaveResult::Committed) {
        finish_request(vault);
    }
    result
}

/// The sign-in's save, once the token it replaces has an owner. Past the account's commit it's
/// `Committed`, whatever the marker writes after it do.
fn commit<S: CredentialStore>(
    vault: &mut Vault<S>,
    account: &Secret<Account>,
    added_at: u64,
) -> Done {
    let listed = match own_replaced(vault, account, added_at) {
        Ok(listed) => listed,
        Err(error) => {
            return Done::Committed {
                result: SaveResult::NotCommitted(error),
                notice_cleared: false,
                listed: None,
            };
        }
    };
    let result = vault.save_account(account);
    let mut notice_cleared = false;
    if matches!(result, SaveResult::Committed) {
        finish_request(vault);
        match vault.update_marker(MarkerUpdate::SignInCommitted) {
            Ok(_) => notice_cleared = true,
            Err(error) => {
                tracing::warn!(%error, "an old sign-out notice stays until the next sign-in");
            }
        }
    }
    Done::Committed {
        result,
        notice_cleared,
        listed,
    }
}

/// Hands the refresh token `chatgpt` holds to a durable owner before `account` replaces it: a
/// list entry, keeping a slot for `account`'s own set should its save be refused, or else
/// `outcome_unknown`. Returns the list once it saved one.
fn own_replaced<S: CredentialStore>(
    vault: &mut Vault<S>,
    account: &Secret<Account>,
    added_at: u64,
) -> Result<Option<Vec<Secret<Revocable>>>, VaultError> {
    let replaced = match vault.load_account().map(Ok).or_else(damaged)? {
        Ok(old) => {
            let Some((client_id, refresh_token)) = old.and_then(|old| {
                let old = old.expose();
                Some((
                    old.client_id.clone(),
                    old.tokens.as_ref()?.refresh_token.clone(),
                ))
            }) else {
                return Ok(None);
            };
            let incoming = account.expose().tokens.as_ref();
            if incoming.is_some_and(|tokens| tokens.refresh_token == refresh_token) {
                return Ok(None);
            }
            Some((client_id, refresh_token))
        }
        Err(_) => None,
    };
    if let Some((client_id, refresh_token)) = replaced
        && let Ok(list) = vault.load_revocations().map(Ok).or_else(damaged)?
    {
        let mut entries = list.map_or_else(Vec::new, |list| list.expose().entries.clone());
        if entries
            .iter()
            .any(|entry| entry.client_id == client_id && entry.refresh_token == refresh_token)
        {
            return Ok(None);
        }
        let exchanges = entries
            .iter()
            .filter(|entry| entry.origin == Origin::Exchange)
            .count();
        if entry_fits(&client_id, &refresh_token) && exchanges + 2 <= MAX_EXCHANGES {
            entries.push(Revocable {
                client_id,
                refresh_token,
                added_at,
                origin: Origin::Exchange,
            });
            let list = Secret::new(RevocationList {
                entries: entries.clone(),
            });
            match vault.save_revocations(&list) {
                SaveResult::Committed => {
                    return Ok(Some(entries.into_iter().map(Secret::new).collect()));
                }
                // Refused before anything was written, and every retry would be.
                SaveResult::NotCommitted(VaultError::Bounds) => {}
                SaveResult::NotCommitted(error) => return Err(error),
            }
        }
    }
    vault.update_marker(MarkerUpdate::UnknownOutcome)?;
    Ok(None)
}

/// The saved result is owned now, so the lane's pending flag can go. A refused write leaves it
/// set, which the next request or start counts as uncertainty: conservative, never false.
fn finish_request<S: CredentialStore>(vault: &mut Vault<S>) {
    if let Err(error) = vault.update_marker(MarkerUpdate::RequestFinished {
        outcome_unknown: false,
    }) {
        tracing::warn!(%error, "the token lane's pending flag stays set");
    }
}

fn expired(entry: &Revocable, now: u64) -> bool {
    now >= entry.added_at.saturating_add(AGE_OUT_SECONDS)
}

/// `chatgpt` never changes before the checkpoint, so a listed `Account` entry, that token or
/// its saved successor, is kept as it is, and so is the token listed already; otherwise the
/// account's token is adopted. A list that can never hold it isn't rewritten: the token is still
/// revoked, and `outcome_unknown`, persisted first, keeps the notice.
fn prepare<S: CredentialStore>(
    vault: &mut Vault<S>,
    added_at: u64,
) -> Result<Vec<Secret<Revocable>>, VaultError> {
    let list = vault.load_revocations().map(Ok).or_else(damaged)?;
    let account = vault.load_for_sign_out().map(Ok).or_else(damaged)?;
    if list.is_err() || account.is_err() {
        vault.update_marker(MarkerUpdate::UnknownOutcome)?;
    }
    let readable = list.is_ok();
    let mut entries = list
        .ok()
        .flatten()
        .map_or_else(Vec::new, |list| list.expose().entries.clone());
    let token = account.ok().flatten().and_then(|account| {
        let account = account.expose();
        let tokens = account.tokens.as_ref()?;
        Some((account.client_id.clone(), tokens.refresh_token.clone()))
    });
    if let Some((client_id, refresh_token)) = token
        && !entries.iter().any(|entry| {
            entry.origin == Origin::Account
                || (entry.client_id == client_id && entry.refresh_token == refresh_token)
        })
    {
        entries.insert(
            0,
            Revocable {
                client_id,
                refresh_token,
                added_at,
                origin: Origin::Account,
            },
        );
        // An unreadable list isn't overwritten: the token is still revoked, and the uncertainty
        // just persisted keeps the notice.
        if readable {
            match vault.save_revocations(&Secret::new(RevocationList {
                entries: entries.clone(),
            })) {
                SaveResult::Committed => {}
                SaveResult::NotCommitted(VaultError::Bounds) => {
                    vault.update_marker(MarkerUpdate::UnknownOutcome)?;
                }
                SaveResult::NotCommitted(error) => return Err(error),
            }
        }
    }
    Ok(entries.into_iter().map(Secret::new).collect())
}

/// Damaged data is given up on, as uncertainty; a refused read stays an error, so the step is
/// refused and Retry reads again.
fn damaged<T>(error: VaultError) -> Result<Result<T, VaultError>, VaultError> {
    if error.is_damage() {
        Ok(Err(error))
    } else {
        Err(error)
    }
}

/// Drops the confirmed entries, then decides the notice. A refused rewrite leaves them listed,
/// which only counts against confirmation.
fn checkpoint<S: CredentialStore>(
    vault: &mut Vault<S>,
    confirmed: &[Secret<Revocable>],
) -> Result<bool, VaultError> {
    let list_empty_on_disk = relist(vault, |entry| {
        confirmed.iter().any(|done| done.expose() == entry)
    })
    .inspect_err(|error| tracing::warn!(%error, "confirmed revocations stay listed"))
    .is_ok_and(|entries| entries.is_empty());
    // Uncertainty Prepare persisted already decides the notice, so the absent-list check, which
    // rereads an account Prepare found damaged, isn't asked for.
    let list_empty_on_disk = list_empty_on_disk && !vault.marker()?.outcome_unknown;
    vault
        .update_marker(MarkerUpdate::Checkpoint { list_empty_on_disk })
        .map(|marker| marker.notice)
}

/// Drops the entries `leaves` picks and saves the rest. Outside a sign-out, whose Delete step
/// removes them instead, an empty list's names are deleted and checked, then the sweep fact clears.
fn relist<S: CredentialStore>(
    vault: &mut Vault<S>,
    leaves: impl Fn(&Revocable) -> bool,
) -> Result<Vec<Secret<Revocable>>, VaultError> {
    let mut entries = vault
        .load_revocations()?
        .map_or_else(Vec::new, |list| list.expose().entries.clone());
    let before = entries.len();
    entries.retain(|entry| !leaves(entry));
    if entries.len() != before {
        let list = Secret::new(RevocationList {
            entries: entries.clone(),
        });
        if let SaveResult::NotCommitted(error) = vault.save_revocations(&list) {
            return Err(error);
        }
    }
    let marker = vault.marker()?;
    if entries.is_empty() && marker.sweep && marker.sign_out.is_none() {
        if let DeleteResult::Refused(error) = vault.delete_revocations() {
            return Err(error);
        }
        vault.update_marker(MarkerUpdate::FinishSweep)?;
    }
    Ok(entries.into_iter().map(Secret::new).collect())
}
