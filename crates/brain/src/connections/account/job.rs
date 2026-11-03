//! Credential work the account state machine asks for, run one job at a time on the
//! credential lane's thread, which owns the vault and its session lock.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use crate::connections::clock::Clock;
use crate::connections::error::VaultError;
use crate::connections::marker::{MarkerUpdate, SignOutMarker};
use crate::connections::records::{Account, Revocable, RevocationList};
use crate::connections::vault::{DeleteResult, SaveResult, Vault};
use crate::platform::credential::{CredentialStore, WindowsCredentialStore};

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
    /// Save a verified sign-in; once committed, release its lane fact and an old notice.
    Commit(Secret<Account>),
    /// Add a received refresh token to the revocation list; once committed, release its lane
    /// fact. Its `added_at` is stamped when it's saved.
    List(Secret<Revocable>),
    /// Drop these confirmed entries, and with `age_out` the expired ones, from the revocation
    /// list. A list left empty has its names deleted and checked, then its sweep fact cleared.
    Relist {
        /// Entries an empty 200 revoked.
        confirmed: Vec<Secret<Revocable>>,
        /// Whether to drop entries past their age-out too.
        age_out: bool,
    },
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Load { .. } => "Load",
            Self::Register(_) => "Register",
            Self::Marker(_) => "Marker",
            Self::Commit(_) => "Commit",
            Self::List(_) => "List",
            Self::Relist { .. } => "Relist",
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
}

/// What the vault held when it was read. Each record fails on its own, so one damaged record
/// doesn't hide the others.
#[derive(Debug)]
pub struct Snapshot {
    /// Control state; a refused read fails closed like an unreadable marker.
    pub marker: SignOutMarker,
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
}

impl<S: CredentialStore> fmt::Debug for Worker<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Worker(***)")
    }
}

impl Worker<WindowsCredentialStore> {
    /// The worker on the real credential store: the only way the brain reaches it outside tests.
    pub fn system(clock: Arc<dyn Clock>) -> Self {
        Self::new(Box::new(|| Vault::open(WindowsCredentialStore)), clock)
    }
}

impl<S: CredentialStore> Worker<S> {
    /// A worker that opens its vault with `open` and stamps revocation entries with `clock`.
    pub fn new(
        open: Box<dyn FnMut() -> Result<Vault<S>, VaultError> + Send>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            vault: None,
            open,
            started: (clock.unix_seconds(), Instant::now()),
            clock,
            jumped: false,
        }
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
        let Some(vault) = self.vault.as_mut() else {
            let refused = || SaveResult::NotCommitted(VaultError::Busy);
            return match job {
                Job::Load { .. } => unreachable!("handled above"),
                Job::Register(_) => Done::Registered(refused()),
                Job::Marker(_) => Done::Marked(Err(VaultError::Busy)),
                Job::Commit(_) => Done::Committed {
                    result: refused(),
                    notice_cleared: false,
                },
                Job::List(_) => Done::Listed {
                    result: refused(),
                    entries: Vec::new(),
                },
                Job::Relist { .. } => Done::Relisted(Err(VaultError::Busy)),
            };
        };
        match job {
            Job::Load { .. } => unreachable!("handled above"),
            Job::Register(account) => Done::Registered(vault.save_account(&account)),
            Job::Marker(update) => Done::Marked(vault.update_marker(update).map(drop)),
            Job::Commit(account) => {
                let result = vault.save_account(&account);
                let mut notice_cleared = false;
                if matches!(result, SaveResult::Committed { .. }) {
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
                }
            }
            Job::List(entry) => {
                let added_at = self.clock.unix_seconds();
                let list = vault.load_revocations().map(|list| {
                    let mut list =
                        list.map_or_else(RevocationList::default, |list| list.expose().clone());
                    let mut entry = entry.expose().clone();
                    // shortcut: a clock before 1970 keeps the entry until a later start sees a
                    // real time; age-out (slice 3 PR 9b part 2) can't evict it early.
                    entry.added_at = added_at.unwrap_or(u64::MAX);
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
                if matches!(result, SaveResult::Committed { .. }) {
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
        }
    }

    /// Wall time for age-out, or `None` while it's suspended: the clock is unreadable, or it
    /// jumped forward since this worker started.
    fn now_for_age_out(&mut self) -> Option<u64> {
        let now = self.clock.unix_seconds()?;
        let (wall, monotonic) = self.started;
        let ran = monotonic.elapsed().as_secs();
        if wall.is_none_or(|wall| now.saturating_sub(wall) > ran.saturating_add(CLOCK_JUMP_SECONDS))
        {
            if !self.jumped {
                tracing::warn!("the wall clock jumped; age-out waits for the next start");
            }
            self.jumped = true;
        }
        (!self.jumped).then_some(now)
    }

    fn load(&mut self, mut tries: u32) -> Result<Snapshot, VaultError> {
        if self.vault.is_none() {
            let vault = loop {
                tries -= 1;
                match (self.open)() {
                    Err(VaultError::Busy) if tries > 0 => std::thread::sleep(BUSY_WAIT),
                    opened => break opened?,
                }
            };
            self.vault = Some(vault);
        }
        let aged = self.now_for_age_out();
        let vault = self.vault.as_mut().expect("opened above");
        let marker = vault.marker().unwrap_or_else(|error| {
            tracing::warn!(%error, "the sign-out marker can't be read; failing closed");
            SignOutMarker::fail_closed()
        });
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
            marker,
            host: vault.host_id(),
            account: vault.load_account(),
            revocable,
        })
    }
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

/// Drops the entries `leaves` picks and saves the rest; an empty list's names are deleted and
/// checked, then the sweep fact clears. Outside a sign-out only.
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
    // Only a saved empty list may be deleted outside a sign-out.
    if entries.is_empty() && vault.marker()?.sweep {
        if let DeleteResult::Refused(error) = vault.delete_revocations() {
            return Err(error);
        }
        vault.update_marker(MarkerUpdate::FinishSweep)?;
    }
    Ok(entries.into_iter().map(Secret::new).collect())
}
