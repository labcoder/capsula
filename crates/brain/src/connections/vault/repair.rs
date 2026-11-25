use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::{Vault, VaultError, decode, generation, valid_host};
use crate::connections::authority::{
    AuthorityError, Current, Kind, repair::RepairAttempt as Owner,
};
use crate::platform::credential::CredentialStore;

pub(super) mod exchange;
pub(super) mod promotion;
pub(crate) use promotion::{CutoffRequest, HistoryBinding, HistoryLink, Work};

const ARMED: u8 = 0;
const DISPATCHED: u8 = 1;
const RETIRED: u8 = 2;

#[derive(Debug)]
#[must_use]
pub(in crate::connections) struct RepairAttempt {
    owner: Owner,
    lease: RepairLease,
    pub(super) intake: ExchangeIntake,
    host: Secret<Zeroizing<String>>,
}

#[derive(Debug)]
pub(super) struct RepairLease(Arc<AtomicU8>);

impl Drop for RepairLease {
    fn drop(&mut self) {
        self.0.store(RETIRED, Ordering::Release);
    }
}

#[derive(Debug)]
pub(super) enum ExchangeIntake {
    Open,
    Held(Secret<Zeroizing<String>>),
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::connections) enum RepairRefusal {
    #[error("the account registration changed")]
    Changed,
    #[error("the registered credential has no missing chunk")]
    Present,
    #[error(transparent)]
    Vault(#[from] VaultError),
}

impl From<AuthorityError> for RepairRefusal {
    fn from(error: AuthorityError) -> Self {
        VaultError::from(error).into()
    }
}

impl<S: CredentialStore> Vault<S> {
    fn owns_repair(&self, attempt: &RepairAttempt) -> bool {
        self.repair_lease
            .as_ref()
            .is_some_and(|lease| Arc::ptr_eq(&lease.0, &attempt.lease.0))
    }

    pub(in crate::connections) fn missing_repair_head(
        &self,
        revision: u64,
    ) -> Result<Current, RepairRefusal> {
        let marker = self.marker()?;
        if marker.request_pending || marker.sign_out.is_some() || marker.sweep {
            return Err(VaultError::Fenced.into());
        }
        let old = self.authority.repair_predecessor()?;
        if old.revision != revision || old.revision == 0 {
            return Err(RepairRefusal::Changed);
        }
        let active = old.active.ok_or(RepairRefusal::Changed)?;
        if active.layout.bytes() == 0 {
            return Err(VaultError::Unreadable.into());
        }
        if !generation::has_missing_chunk(&self.store, &self.service, &active)? {
            return Err(RepairRefusal::Present);
        }
        Ok(old)
    }

    pub(in crate::connections) fn confirm_repair(
        &mut self,
        expected: Current,
        claim: impl FnOnce() -> bool,
    ) -> Result<RepairAttempt, RepairRefusal> {
        let old = self.missing_repair_head(expected.revision)?;
        if old != expected {
            return Err(RepairRefusal::Changed);
        }
        let host = self.authority.current(Kind::Host)?;
        let host_ref = host.active.ok_or(VaultError::Fenced)?;
        let bytes = generation::read(&self.store, &self.service, &host_ref)?;
        let value: Zeroizing<String> = decode(&bytes, 128)?;
        if !valid_host(&value) {
            return Err(VaultError::Unreadable.into());
        }
        let revocations = self.authority.current(Kind::Revocations)?;
        if self
            .load_revocations()?
            .is_some_and(|list| !list.expose().entries.is_empty())
        {
            return Err(VaultError::Fenced.into());
        }
        if !claim() {
            return Err(RepairRefusal::Changed);
        }
        let owner = self.authority.start_repair(old, host, revocations)?;
        let dispatch = Arc::new(AtomicU8::new(ARMED));
        self.repair_lease = Some(RepairLease(Arc::clone(&dispatch)));
        Ok(RepairAttempt {
            owner,
            lease: RepairLease(dispatch),
            intake: ExchangeIntake::Open,
            host: Secret::new(value),
        })
    }

    pub(in crate::connections) fn cancel_repair_admission(
        &mut self,
        attempt: &RepairAttempt,
    ) -> Result<(), VaultError> {
        if !self.owns_repair(attempt) {
            return Err(VaultError::Authority);
        }
        attempt.lease.0.store(RETIRED, Ordering::Release);
        match self.authority.cancel_repair_admission(&attempt.owner) {
            Err(AuthorityError::Conflict) => {
                Ok(self.authority.seal_repair_exchange(&attempt.owner)?)
            }
            result => Ok(result?),
        }
    }
}
