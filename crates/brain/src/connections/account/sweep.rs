//! Revocation outside a sign-out (design §2, §5): a pass revokes what's listed and drops what
//! was confirmed. Passes run at startup, when an entry is added and on Retry, never on a timer,
//! one at a time; a request for another while one runs is folded into one more after it.

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ErrorKey};

use super::{AccountState, Effect, Queued, error, exchanges};
use crate::connections::error::VaultError;
use crate::connections::records::Revocable;

/// Where the pass stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    Idle,
    /// Reading the list, for a Retry.
    Reading,
    /// The revocation requests are out.
    Revoking,
    /// Dropping the confirmed entries.
    Relisting,
}

/// Why the list is being rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    /// A full list's age-out check before a sign-in is refused.
    Admit,
    /// A Retry's pass reads what to revoke.
    Read,
    /// The pass confirmed some entries.
    Unlist,
}

impl AccountState {
    /// A pass over the list as it is now, for a Retry.
    pub(super) fn sweep(&mut self) {
        if self.ready_for_pass() {
            self.pass = Pass::Reading;
            self.jobs.push_back(Queued::Relist {
                confirmed: Vec::new(),
                why: Why::Read,
            });
        }
    }

    /// A pass over `entries`, the list just read or saved.
    pub(super) fn revoke(&mut self, entries: Vec<Secret<Revocable>>, effects: &mut Vec<Effect>) {
        if !entries.is_empty() && self.ready_for_pass() {
            self.pass = Pass::Revoking;
            effects.push(Effect::Revoke(entries));
        }
    }

    fn ready_for_pass(&mut self) -> bool {
        if self.busy || self.leaving.is_some() {
            return false;
        }
        if self.pass != Pass::Idle {
            self.again = true;
            return false;
        }
        true
    }

    pub(super) fn revoked(&mut self, confirmed: Vec<Secret<Revocable>>) {
        if self.pass != Pass::Revoking {
            return self.revoked_for_sign_out(confirmed);
        }
        if confirmed.is_empty() {
            return self.end_pass();
        }
        self.pass = Pass::Relisting;
        self.jobs.push_back(Queued::Relist {
            confirmed,
            why: Why::Unlist,
        });
    }

    pub(super) fn relisted(
        &mut self,
        why: Why,
        result: Result<Vec<Secret<Revocable>>, VaultError>,
        effects: &mut Vec<Effect>,
    ) {
        let entries = result
            .inspect_err(|failure| tracing::warn!(%failure, "the revocation list wasn't rewritten"))
            .ok();
        if let Some(entries) = &entries {
            self.loaded.exchanges = exchanges(entries);
        }
        match why {
            Why::Admit => {
                if let Some(intent) = self.admitting.take() {
                    if self.full() {
                        effects.push(error(ErrorKey::RevocationBacklog, Action::Retry));
                    } else {
                        self.sign_in(intent, effects);
                    }
                }
            }
            Why::Read => {
                self.pass = Pass::Idle;
                match entries {
                    Some(entries) if !entries.is_empty() => self.revoke(entries, effects),
                    _ => self.end_pass(),
                }
            }
            Why::Unlist => self.end_pass(),
        }
    }

    fn end_pass(&mut self) {
        self.pass = Pass::Idle;
        if std::mem::take(&mut self.again) {
            self.sweep();
        }
    }
}
