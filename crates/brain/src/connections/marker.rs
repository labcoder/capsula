//! Versioned, token-free control state; updates merge into the current persisted marker.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::error::VaultError;

/// Whether sign-out retains the verified registration or erases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// Retain only the client and subject.
    Keep,
    /// Remove the whole account record.
    Erase,
    /// Erase a saved sign-in this computer can't read, which the person chose to give up: Prepare
    /// gives up its missing or wrong-length chunks too. The checkpoint turns it into `Erase`.
    Discard,
}

/// The durable checkpoint separating revocation from irreversible local deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Prepare and revoke before deciding the outcome at the checkpoint.
    Revoking,
    /// The outcome is decided; only delete and finish may be replayed.
    Deleting,
}

/// A pending sign-out, independent of all token-bearing records.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignOut {
    /// Whether the registration survives.
    pub disposition: Disposition,
    /// Which storage steps are safe to replay.
    pub phase: Phase,
}

/// The compact version-one `signout` credential, never a token-bearing value.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent durable facts in the approved marker layout"
)]
pub struct SignOutMarker {
    v: u8,
    /// Only a pending sign-out fences use of the account.
    #[serde(deserialize_with = "Option::deserialize")]
    pub sign_out: Option<SignOut>,
    /// Token-bearing leftovers still require a by-name sweep.
    pub sweep: bool,
    /// A request's token-producing result is not yet durably owned.
    pub request_pending: bool,
    /// A request may have issued tokens that nobody holds.
    pub outcome_unknown: bool,
    /// The pessimistic remote-revocation warning, decided before deletion.
    pub notice: bool,
}

/// A control-state change, applied to the current marker rather than an old snapshot.
#[derive(Debug, Clone, Copy)]
pub enum MarkerUpdate {
    /// Persist before writing revocation data.
    BeginSweep,
    /// A completed sweep may clear only its own fact, and cannot undo a sign-out promotion.
    FinishSweep,
    /// Persist before dispatching a token-producing request.
    RequestStarted,
    /// Release the request fact only once its result is durably owned.
    RequestFinished {
        /// Whether that result remains ambiguous.
        outcome_unknown: bool,
    },
    /// Keep a sticky unknown-outcome fact, such as for an unreadable record.
    UnknownOutcome,
    /// Promote existing sweep or notice state to a pessimistic sign-out.
    Promote(Disposition),
    /// Decide the remote outcome once, before the first deletion.
    Checkpoint {
        /// Evidence gathered from the persisted list before anything was deleted.
        list_empty_on_disk: bool,
    },
    /// Fold the decided outcome into a notice-only marker, or remove the empty marker.
    FinishSignOut,
    /// Clear an old notice after a new account token set has committed, keeping other facts.
    SignInCommitted,
}

impl Default for SignOutMarker {
    fn default() -> Self {
        Self {
            v: 1,
            sign_out: None,
            sweep: false,
            request_pending: false,
            outcome_unknown: false,
            notice: false,
        }
    }
}

impl SignOutMarker {
    pub(super) fn valid(self) -> bool {
        self.v == 1
            && self.sign_out.is_none_or(|sign_out| {
                self.sweep
                    && match sign_out.phase {
                        Phase::Revoking => self.notice,
                        Phase::Deleting => !self.request_pending && !self.outcome_unknown,
                    }
            })
    }

    pub(super) fn fail_closed() -> Self {
        Self {
            sign_out: Some(SignOut {
                disposition: Disposition::Erase,
                phase: Phase::Revoking,
            }),
            sweep: true,
            outcome_unknown: true,
            notice: true,
            ..Self::default()
        }
    }

    pub(super) fn merge(mut self, update: MarkerUpdate) -> Result<Self, VaultError> {
        match update {
            MarkerUpdate::BeginSweep => self.sweep = true,
            MarkerUpdate::FinishSweep => {
                if self.sign_out.is_none() {
                    self.sweep = false;
                }
            }
            MarkerUpdate::RequestStarted => {
                if self.sign_out.is_some() {
                    return Err(VaultError::Fenced);
                }
                // A pending flag still set here belongs to a request whose result was never
                // marked owned, so it stays as uncertainty instead of being reused.
                self.outcome_unknown |= self.request_pending;
                self.request_pending = true;
            }
            MarkerUpdate::RequestFinished { outcome_unknown } => {
                self.require_not_deleting()?;
                self.request_pending = false;
                self.outcome_unknown |= outcome_unknown;
            }
            MarkerUpdate::UnknownOutcome => {
                self.require_not_deleting()?;
                self.outcome_unknown = true;
            }
            MarkerUpdate::Promote(disposition) => {
                if let Some(existing) = self.sign_out {
                    if existing.disposition != disposition {
                        return Err(VaultError::Fenced);
                    }
                } else {
                    self.sign_out = Some(SignOut {
                        disposition,
                        phase: Phase::Revoking,
                    });
                    self.sweep = true;
                    self.notice = true;
                }
            }
            MarkerUpdate::Checkpoint { list_empty_on_disk } => {
                let sign_out = self.sign_out.as_mut().ok_or(VaultError::Fenced)?;
                if sign_out.phase == Phase::Revoking {
                    self.notice =
                        !list_empty_on_disk || self.outcome_unknown || self.request_pending;
                    self.request_pending = false;
                    self.outcome_unknown = false;
                    sign_out.phase = Phase::Deleting;
                    // The exemption is spent, and an older reader understands only Erase.
                    if sign_out.disposition == Disposition::Discard {
                        sign_out.disposition = Disposition::Erase;
                    }
                }
            }
            MarkerUpdate::FinishSignOut => {
                if self
                    .sign_out
                    .is_none_or(|sign_out| sign_out.phase != Phase::Deleting)
                {
                    return Err(VaultError::Fenced);
                }
                self.sign_out = None;
                self.sweep = false;
            }
            MarkerUpdate::SignInCommitted => {
                if self.sign_out.is_some() {
                    return Err(VaultError::Fenced);
                }
                self.notice = false;
            }
        }
        Ok(self)
    }

    fn require_not_deleting(self) -> Result<(), VaultError> {
        if self
            .sign_out
            .is_some_and(|sign_out| sign_out.phase == Phase::Deleting)
        {
            return Err(VaultError::Fenced);
        }
        Ok(())
    }
}

impl fmt::Debug for SignOutMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignOutMarker(***)")
    }
}

impl fmt::Debug for SignOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignOut(***)")
    }
}
