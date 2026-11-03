//! Credential work the account state machine asks for, run one job at a time on the
//! credential lane's thread, which owns the vault and its session lock.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use crate::connections::clock::Clock;
use crate::connections::error::VaultError;
use crate::connections::marker::{MarkerUpdate, SignOutMarker};
use crate::connections::records::{Account, Origin, Revocable, RevocationList};
use crate::connections::vault::{SaveResult, Vault};
use crate::platform::credential::{CredentialStore, WindowsCredentialStore};

/// How often a patient load tries the session lock [guess]: Windows releases a dead brain's
/// lock a little after the process ends.
const BUSY_TRIES: u32 = 5;
/// The wait between those tries [guess].
const BUSY_WAIT: Duration = Duration::from_millis(250);

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
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Load { .. } => "Load",
            Self::Register(_) => "Register",
            Self::Marker(_) => "Marker",
            Self::Commit(_) => "Commit",
            Self::List(_) => "List",
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
    /// The revocation list's save, and how many exchange entries it holds once committed.
    Listed {
        /// The save.
        result: SaveResult,
        /// Exchange entries in the saved list.
        exchanges: usize,
    },
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
    /// Exchange entries in the revocation list.
    pub exchanges: Result<usize, VaultError>,
}

/// Runs jobs against the vault it opens on the first load, and keeps it, with its session
/// lock, until dropped.
pub struct Worker<S: CredentialStore> {
    vault: Option<Vault<S>>,
    open: Box<dyn FnMut() -> Result<Vault<S>, VaultError> + Send>,
    clock: Arc<dyn Clock>,
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
            clock,
        }
    }

    /// Runs one job to its end. Blocking: call it off the core loop.
    pub fn run(&mut self, job: Job) -> Done {
        if let Job::Load { patient } = job {
            let tries = if patient { BUSY_TRIES } else { 1 };
            return Done::Loaded(self.load(tries).map(Box::new));
        }
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
                    exchanges: 0,
                },
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
                            exchanges: 0,
                        };
                    }
                };
                let exchanges = exchanges(&list);
                let result = vault.save_revocations(&Secret::new(list));
                if matches!(result, SaveResult::Committed { .. }) {
                    finish_request(vault);
                }
                Done::Listed { result, exchanges }
            }
        }
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
        let vault = self.vault.as_mut().expect("opened above");
        let marker = vault.marker().unwrap_or_else(|error| {
            tracing::warn!(%error, "the sign-out marker can't be read; failing closed");
            SignOutMarker::fail_closed()
        });
        Ok(Snapshot {
            marker,
            host: vault.host_id(),
            account: vault.load_account(),
            exchanges: vault
                .load_revocations()
                .map(|list| list.map_or(0, |list| exchanges(list.expose()))),
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

fn exchanges(list: &RevocationList) -> usize {
    list.entries
        .iter()
        .filter(|entry| entry.origin == Origin::Exchange)
        .count()
}
