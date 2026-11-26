use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::{ARMED, DISPATCHED, ExchangeIntake, RepairAttempt};
use crate::connections::authority::repair::ExchangeStep;
use crate::connections::error::VaultError;
use crate::connections::http_client::Http;
use crate::connections::oauth::{
    Authorization,
    exchange::{ExchangeError, ExchangeRequest, prepare_exchange},
};
use crate::connections::records::{MAX_ID_BYTES, MAX_REFRESH_BYTES};
use crate::connections::vault::{Vault, generation};
use crate::platform::credential::{CredentialStore, SecretBytes};

#[derive(Debug)]
#[must_use]
pub(in crate::connections) struct RepairDispatch {
    owner: Arc<AtomicU8>,
    request: ExchangeRequest,
}

#[derive(Debug)]
#[must_use]
pub(in crate::connections) struct RepairReceipt {
    owner: Arc<AtomicU8>,
    result: Option<Receipt>,
}

#[derive(Debug)]
enum Receipt {
    NotSent,
    Unknown,
    Issued(Secret<Zeroizing<String>>),
}

#[derive(Debug)]
pub(in crate::connections) enum RepairSettlement {
    NotSent,
    Unknown,
    Escrowed,
}

impl RepairDispatch {
    pub(in crate::connections) async fn receive(self, http: &impl Http) -> RepairReceipt {
        let result = if self
            .owner
            .compare_exchange(ARMED, DISPATCHED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            match self.request.send(http).await {
                Ok(tokens) => tokens.refresh_token.map_or(Receipt::Unknown, Receipt::Issued),
                Err(ExchangeError::Unusable { refresh_token }) => {
                    refresh_token.map_or(Receipt::Unknown, Receipt::Issued)
                }
                Err(_) => Receipt::Unknown,
            }
        } else {
            Receipt::NotSent
        };
        RepairReceipt {
            owner: self.owner,
            result: Some(result),
        }
    }
}

impl<S: CredentialStore> Vault<S> {
    pub(in crate::connections) fn charge_repair_exchange(
        &mut self,
        attempt: &RepairAttempt,
        authorization: &Authorization,
    ) -> Result<RepairDispatch, VaultError> {
        if !self.owns_repair(attempt) || attempt.lease.0.load(Ordering::Acquire) != ARMED {
            return Err(VaultError::Fenced);
        }
        let request = prepare_exchange(authorization).map_err(|_| VaultError::Fenced)?;
        let bytes = encode_escrow(authorization.client_id.expose(), "")?;
        self.write_repair_escrow(attempt, ExchangeStep::Charge, &bytes)?;
        Ok(RepairDispatch {
            owner: Arc::clone(&attempt.lease.0),
            request,
        })
    }

    pub(in crate::connections) fn settle_repair_exchange(
        &mut self,
        attempt: &mut RepairAttempt,
        receipt: &mut RepairReceipt,
    ) -> Result<RepairSettlement, VaultError> {
        if !self.owns_repair(attempt)
            || !Arc::ptr_eq(&receipt.owner, &attempt.lease.0)
            || !matches!(attempt.intake, ExchangeIntake::Open)
        {
            return Err(VaultError::Fenced);
        }
        let active = self.authority.charged_repair_client(&attempt.owner)?;
        let result = receipt.result.take().ok_or(VaultError::Fenced)?;
        let outcome = match result {
            Receipt::Issued(handle) => {
                attempt.intake = ExchangeIntake::Held(handle);
                let settled = (|| {
                    let client = generation::read(&self.store, &self.service, &active)?;
                    let (client, previous) = decode_escrow(client.expose())?;
                    let ExchangeIntake::Held(handle) = &attempt.intake else {
                        return Err(VaultError::Fenced);
                    };
                    if !previous.is_empty() || handle.expose().is_empty() {
                        return Err(VaultError::Unreadable);
                    }
                    let bytes = encode_escrow(client, handle.expose())?;
                    self.write_repair_escrow(attempt, ExchangeStep::Drain, &bytes)
                })();
                if let Err(error) = settled {
                    self.authority.settle_repair_uncertain(&attempt.owner)?;
                    return Err(error);
                }
                RepairSettlement::Escrowed
            }
            Receipt::NotSent | Receipt::Unknown => {
                attempt.intake = ExchangeIntake::Closed;
                self.authority.settle_repair_uncertain(&attempt.owner)?;
                if matches!(result, Receipt::NotSent) {
                    RepairSettlement::NotSent
                } else {
                    RepairSettlement::Unknown
                }
            }
        };
        attempt.intake = ExchangeIntake::Closed;
        Ok(outcome)
    }

    fn write_repair_escrow(
        &mut self,
        attempt: &RepairAttempt,
        step: ExchangeStep,
        bytes: &SecretBytes,
    ) -> Result<(), VaultError> {
        let plan =
            self.authority
                .stage_repair_credential(&attempt.owner, step, bytes.expose().len())?;
        let verified = generation::write(&self.store, &self.service, plan, bytes)?;
        Ok(self
            .authority
            .commit_repair_credential(&attempt.owner, step, verified)?)
    }
}

fn encode_escrow(client: &str, handle: &str) -> Result<SecretBytes, VaultError> {
    if client.is_empty() || client.len() > MAX_ID_BYTES || handle.len() > MAX_REFRESH_BYTES {
        return Err(VaultError::Bounds);
    }
    let len = u16::try_from(client.len()).map_err(|_| VaultError::Bounds)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(2 + client.len() + handle.len()));
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(client.as_bytes());
    bytes.extend_from_slice(handle.as_bytes());
    Ok(Secret::new(bytes))
}

fn decode_escrow(bytes: &[u8]) -> Result<(&str, &str), VaultError> {
    let (len, bytes) = bytes
        .split_first_chunk::<2>()
        .ok_or(VaultError::Unreadable)?;
    let (client, handle) = bytes
        .split_at_checked(usize::from(u16::from_le_bytes(*len)))
        .ok_or(VaultError::Unreadable)?;
    if client.is_empty() || client.len() > MAX_ID_BYTES || handle.len() > MAX_REFRESH_BYTES {
        return Err(VaultError::Unreadable);
    }
    Ok((
        str::from_utf8(client).map_err(|_| VaultError::Unreadable)?,
        str::from_utf8(handle).map_err(|_| VaultError::Unreadable)?,
    ))
}
