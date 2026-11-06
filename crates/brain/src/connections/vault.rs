//! Locked, synchronous account records. The credential authority decides which write-once
//! generation of each record is current; Credential Manager only holds the bytes (OSC-233).

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{CREDENTIAL_SERVICE, account_targets, anthropic_staging_target};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::authority::{self, Authority, Kind, io as generation};
use super::error::VaultError;
use super::marker::{Disposition, MarkerUpdate, Phase, SignOutMarker};
use super::records::{Account, MAX_RECORD_BYTES, Record, RevocationList};
use crate::platform::credential::{CredentialStore, SecretBytes};
use crate::platform::{self, SessionLock};

mod github;
pub(crate) mod staging;

/// Removed generations one sweep pass visits [guess]; the rest wait for the next pass.
const SWEEP: usize = 16;

/// A save's durable outcome.
#[derive(Debug)]
#[must_use]
pub enum SaveResult {
    /// The new record is the current generation.
    Committed,
    /// Nothing changed; the caller still owns the unsaved value.
    NotCommitted(VaultError),
}

/// Whether a record's removal committed.
#[derive(Debug)]
#[must_use]
pub enum DeleteResult {
    /// The record is removed for good; its names are swept in the background.
    Gone,
    /// Nothing was removed; retry must not claim success yet.
    Refused(VaultError),
}

/// Owns the session lock while its store is accessible, including during blocking work.
///
/// Move the vault into blocking work, rather than dropping its guard when an async waiter ends.
#[must_use = "the vault must retain its session lock until all blocking work ends"]
pub struct Vault<S: CredentialStore> {
    authority: Authority,
    store: S,
    service: String,
    paste: Option<authority::WritePlan>,
    _session: SessionLock,
}

impl<S: CredentialStore> fmt::Debug for Vault<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(***)")
    }
}

impl<S: CredentialStore> Vault<S> {
    pub(crate) fn anthropic(
        &mut self,
        op: super::anthropic::KeyOp,
    ) -> Result<Option<Secret<super::anthropic::key::AnthropicKey>>, super::anthropic::KeyError>
    {
        use super::anthropic::{KeyError, KeyOp, key::AnthropicKey};
        match op {
            KeyOp::Remove => {
                let paste = self.paste.take();
                let result = self.remove(&[Kind::AnthropicKey], None);
                if let Some(plan) = paste {
                    generation::delete_pending(
                        &self.store,
                        &self.service,
                        Kind::AnthropicKey,
                        plan.generation(),
                        plan.layout(),
                    );
                }
                result?;
                Ok(None)
            }
            KeyOp::Load => self
                .read_bytes(Kind::AnthropicKey)?
                .map(AnthropicKey::parse)
                .transpose()
                .map_err(KeyError::Rejected),
        }
    }

    /// Takes account-session ownership without reading any credential yet.
    ///
    /// # Errors
    /// Returns typed Busy for contention, or a path/lock failure otherwise.
    pub(in crate::connections) fn open(store: S) -> Result<Self, VaultError> {
        let path = platform::session_lock_path().ok_or(VaultError::NoDataDir)?;
        Self::with_service(store, CREDENTIAL_SERVICE, &path)
    }

    /// Takes the lock at `path`, then opens the authority beside it and sweeps once, legacy
    /// names included.
    pub(crate) fn with_service(store: S, service: &str, path: &Path) -> Result<Self, VaultError> {
        let session = SessionLock::try_acquire(path)?;
        let authority = Authority::open(&path.with_file_name(authority::FILE), &session)?;
        let mut vault = Self {
            authority,
            store,
            service: service.to_owned(),
            paste: None,
            _session: session,
        };
        vault.sweep_legacy();
        vault.sweep();
        Ok(vault)
    }

    /// Reads or creates the machine-local random `UUIDv4` host identifier. A current host whose
    /// bytes don't read back is an error, never replaced.
    ///
    /// # Errors
    /// Reports unreadable data, unavailable randomness or any refused credential operation.
    pub fn host_id(&mut self) -> Result<Secret<Zeroizing<String>>, VaultError> {
        if let Some(bytes) = self.read_bytes(Kind::Host)? {
            let host: Zeroizing<String> = decode(&bytes, 128)?;
            if !valid_host(&host) {
                return Err(VaultError::Unreadable);
            }
            return Ok(Secret::new(host));
        }
        let mut bytes = Zeroizing::new([0u8; 16]);
        getrandom::fill(&mut *bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let mut host = Zeroizing::new(String::with_capacity(45));
        host.push_str("urn:uuid:");
        for (index, byte) in bytes.iter().enumerate() {
            use std::fmt::Write as _;
            if [4, 6, 8, 10].contains(&index) {
                host.push('-');
            }
            write!(&mut *host, "{byte:02x}").expect("writing into a String cannot fail");
        }
        self.write(Kind::Host, &encode(&host, 128)?, None)?;
        Ok(Secret::new(host))
    }

    /// Reads the marker first, so a pending or unreadable sign-out never exposes usable tokens.
    ///
    /// # Errors
    /// Reports refused reads and malformed or oversized account data.
    pub fn load_account(&self) -> Result<Option<Secret<Account>>, VaultError> {
        if self.marker()?.sign_out.is_some() {
            return Ok(None);
        }
        self.read(Kind::Account)
    }

    /// Reads the fenced account only for sign-out preparation and registration-only deletion.
    ///
    /// This never authorizes inference or refresh; ordinary use must call [`Self::load_account`].
    ///
    /// # Errors
    /// Refuses without a pending sign-out, or reports refused reads and malformed account data.
    pub fn load_for_sign_out(&self) -> Result<Option<Secret<Account>>, VaultError> {
        if self.marker()?.sign_out.is_none() {
            return Err(VaultError::Fenced);
        }
        self.read(Kind::Account)
    }

    /// Replaces the account with a fresh generation.
    pub fn save_account(&mut self, account: &Secret<Account>) -> SaveResult {
        let result = (|| {
            account.expose().validate()?;
            let bytes = encode(account.expose(), MAX_RECORD_BYTES)?;
            if let Some(sign_out) = self.marker()?.sign_out {
                let account = account.expose();
                if sign_out.phase != Phase::Deleting
                    || sign_out.disposition != Disposition::Keep
                    || account.tokens.is_some()
                    || account.email.is_some()
                    || account.plan_enabled
                {
                    return Err(VaultError::Fenced);
                }
            }
            self.write(Kind::Account, &bytes, None)
        })();
        saved(result)
    }

    /// Reads revocation-only tokens without making them usable as account tokens.
    ///
    /// # Errors
    /// Reports refused reads and malformed, incomplete or oversized revocation records.
    pub fn load_revocations(&self) -> Result<Option<Secret<RevocationList>>, VaultError> {
        self.read(Kind::Revocations)
    }

    /// Commits the revocation record and the sweep flag together.
    pub fn save_revocations(&mut self, list: &Secret<RevocationList>) -> SaveResult {
        let result = (|| {
            list.expose().validate()?;
            let bytes = encode(list.expose(), MAX_RECORD_BYTES)?;
            let marker = self.marker()?;
            if marker
                .sign_out
                .is_some_and(|sign_out| sign_out.phase == Phase::Deleting)
            {
                return Err(VaultError::Fenced);
            }
            let next = marker.merge(MarkerUpdate::BeginSweep)?;
            self.write(Kind::Revocations, &bytes, (next != marker).then_some(next))
        })();
        saved(result)
    }

    /// Reads token-free control state; malformed or unknown layouts fail closed.
    ///
    /// # Errors
    /// Reports an unavailable or damaged authority; a missing marker is never the default.
    pub fn marker(&self) -> Result<SignOutMarker, VaultError> {
        Ok(self.authority.marker()?)
    }

    /// Merges a change into the current persisted marker under the held session lock.
    ///
    /// `Checkpoint` takes evidence from the caller's saved revocation results, before any delete.
    /// Its decision is immutable once the persisted phase is Deleting.
    ///
    /// # Errors
    /// Reports incompatible phases, a refused commit, or a record that's still current.
    pub fn update_marker(&mut self, update: MarkerUpdate) -> Result<SignOutMarker, VaultError> {
        let current = self.marker()?;
        match update {
            MarkerUpdate::SignInCommitted => {
                if current.sign_out.is_some()
                    || self
                        .read::<Account>(Kind::Account)?
                        .is_none_or(|account| account.expose().tokens.is_none())
                {
                    return Err(VaultError::Fenced);
                }
            }
            MarkerUpdate::Checkpoint {
                list_empty_on_disk: true,
            } if current
                .sign_out
                .is_some_and(|sign_out| sign_out.phase == Phase::Revoking) =>
            {
                match self.load_revocations()? {
                    Some(list) if !list.expose().entries.is_empty() => {
                        return Err(VaultError::Fenced);
                    }
                    None => {
                        if self
                            .read::<Account>(Kind::Account)?
                            .is_some_and(|account| account.expose().tokens.is_some())
                        {
                            return Err(VaultError::Fenced);
                        }
                    }
                    Some(_) => {}
                }
            }
            MarkerUpdate::FinishSweep if current.sign_out.is_none() => {
                self.assert_removed(Kind::Revocations)?;
            }
            MarkerUpdate::FinishSignOut => self.assert_sign_out_clean(current)?,
            _ => {}
        }
        let next = current.merge(update)?;
        if next != current {
            self.authority.set_marker(next)?;
        }
        Ok(next)
    }

    /// Removes the account only after a persisted deletion checkpoint.
    pub fn delete_account(&mut self) -> DeleteResult {
        let result = (|| {
            let marker = self.marker()?;
            if marker
                .sign_out
                .is_none_or(|sign_out| sign_out.phase != Phase::Deleting)
            {
                return Err(VaultError::Fenced);
            }
            self.remove(&[Kind::Account], None)
        })();
        deleted(result)
    }

    /// Removes the revocation record while retaining the independent sweep/sign-out marker.
    /// Outside sign-out, the saved list must be empty, or malformed with persisted uncertainty.
    pub fn delete_revocations(&mut self) -> DeleteResult {
        let result = (|| {
            let marker = self.marker()?;
            if !marker.sweep
                || marker
                    .sign_out
                    .is_some_and(|sign_out| sign_out.phase != Phase::Deleting)
            {
                return Err(VaultError::Fenced);
            }
            if marker.sign_out.is_none() {
                match self.load_revocations() {
                    Ok(Some(list)) if !list.expose().entries.is_empty() => {
                        return Err(VaultError::Fenced);
                    }
                    Ok(_) => {}
                    Err(error) if error.is_damage() && marker.outcome_unknown => {}
                    Err(error) => return Err(error),
                }
            }
            self.remove(&[Kind::Revocations], None)
        })();
        deleted(result)
    }

    /// Removes both records after the checkpoint, driven by the marker alone. Keep retains a
    /// readable registration, rewritten without tokens; with none, the account is removed too.
    pub fn delete_for_sign_out(&mut self) -> DeleteResult {
        let result = (|| {
            let sign_out = self
                .marker()?
                .sign_out
                .filter(|sign_out| sign_out.phase == Phase::Deleting)
                .ok_or(VaultError::Fenced)?;
            if sign_out.disposition == Disposition::Keep && self.keep_registration()? {
                self.remove(&[Kind::Revocations], None)
            } else {
                self.remove(&[Kind::Account, Kind::Revocations], None)
            }
        })();
        deleted(result)
    }

    /// Rewrites the account to its registration. False when there's no registration or its data
    /// is damaged, so the caller removes it instead.
    fn keep_registration(&mut self) -> Result<bool, VaultError> {
        // A refused read is retried, never taken as a reason to erase the registration.
        let Some(account) = self.read::<Account>(Kind::Account).or_else(|error| {
            if error.is_damage() {
                Ok(None)
            } else {
                Err(error)
            }
        })?
        else {
            return Ok(false);
        };
        let account = account.expose();
        let registration = Account {
            client_id: account.client_id.clone(),
            subject: account.subject.clone(),
            email: None,
            plan_enabled: false,
            tokens: None,
        };
        if *account != registration {
            self.write(
                Kind::Account,
                &encode(&registration, MAX_RECORD_BYTES)?,
                None,
            )?;
        }
        Ok(true)
    }

    /// Seeds a current key through the same authority path as an adopted paste.
    #[cfg(test)]
    pub(crate) fn save_anthropic_key(&mut self, key: &[u8]) -> Result<(), VaultError> {
        self.write(
            Kind::AnthropicKey,
            &Secret::new(Zeroizing::new(key.to_vec())),
            None,
        )
    }

    fn read_bytes(&self, kind: Kind) -> Result<Option<SecretBytes>, VaultError> {
        let Some(active) = self.authority.current(kind)?.active else {
            return Ok(None);
        };
        Ok(Some(generation::read(&self.store, &self.service, &active)?))
    }

    fn read<T: Record + DeserializeOwned>(
        &self,
        kind: Kind,
    ) -> Result<Option<Secret<T>>, VaultError> {
        let Some(bytes) = self.read_bytes(kind)? else {
            return Ok(None);
        };
        let value: T = decode(&bytes, MAX_RECORD_BYTES)?;
        value.validate()?;
        Ok(Some(Secret::new(value)))
    }

    /// Writes a fresh generation and makes it current, with `marker` in the same commit.
    fn write(
        &mut self,
        kind: Kind,
        bytes: &SecretBytes,
        marker: Option<SignOutMarker>,
    ) -> Result<(), VaultError> {
        let revision = self.authority.current(kind)?.revision;
        self.write_expected(kind, revision, bytes, marker)
    }

    fn write_expected(
        &mut self,
        kind: Kind,
        revision: u64,
        bytes: &SecretBytes,
        marker: Option<SignOutMarker>,
    ) -> Result<(), VaultError> {
        let plan = self.authority.begin(kind, revision, bytes.expose().len())?;
        let candidate = (plan.generation(), plan.layout());
        let result = generation::write(&self.store, &self.service, plan, bytes)
            .and_then(|verified| self.authority.activate(verified, marker));
        if let Err(error) = result {
            generation::delete_pending(&self.store, &self.service, kind, candidate.0, candidate.1);
            return Err(error.into());
        }
        self.sweep();
        Ok(())
    }

    fn remove(&mut self, kinds: &[Kind], marker: Option<SignOutMarker>) -> Result<(), VaultError> {
        self.authority.remove(kinds, marker)?;
        self.sweep();
        Ok(())
    }

    fn assert_removed(&self, kind: Kind) -> Result<(), VaultError> {
        if self.authority.current(kind)?.active.is_some() {
            return Err(VaultError::NotGone);
        }
        Ok(())
    }

    fn assert_sign_out_clean(&self, marker: SignOutMarker) -> Result<(), VaultError> {
        let sign_out = marker.sign_out.ok_or(VaultError::Fenced)?;
        if sign_out.phase != Phase::Deleting {
            return Err(VaultError::Fenced);
        }
        self.assert_removed(Kind::Revocations)?;
        match sign_out.disposition {
            Disposition::Erase => self.assert_removed(Kind::Account),
            Disposition::Keep => match self.read::<Account>(Kind::Account)? {
                Some(account)
                    if account.expose().tokens.is_some()
                        || account.expose().email.is_some()
                        || account.expose().plan_enabled =>
                {
                    Err(VaultError::Fenced)
                }
                _ => Ok(()),
            },
        }
    }

    /// Deletes the 262 names earlier builds wrote, never reading one, until a clean pass in any
    /// open after the first one ends it; a refused delete waits for the next open. Every open
    /// also deletes the old staging name as defense against earlier writers.
    fn sweep_legacy(&mut self) {
        match self.authority.legacy_owed() {
            Ok(true) => {}
            Ok(false) => {
                if self
                    .store
                    .delete(&anthropic_staging_target(&self.service))
                    .is_err()
                {
                    tracing::warn!("a staged key paste wasn't deleted");
                }
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "earlier credential names weren't checked for cleanup");
                return;
            }
        }
        let refused = account_targets(&self.service)
            .filter(|name| self.store.delete(name).is_err())
            .count();
        if refused > 0 {
            tracing::warn!(refused, "earlier credential names weren't all deleted");
        }
        if let Err(error) = self.authority.legacy_deleted(refused) {
            tracing::warn!(%error, "an earlier credential cleanup wasn't recorded");
        }
    }

    /// One bounded pass over removed generations' names. A failure stays owed to a later pass,
    /// and nothing the store returns changes which generation is current.
    fn sweep(&mut self) {
        let batch = match self.authority.retired(SWEEP) {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!(%error, "removed credentials weren't listed for cleanup");
                return;
            }
        };
        for retired in &batch {
            let observed = generation::sweep(&self.store, &self.service, retired);
            if let Err(error) = self.authority.observed(retired, observed) {
                tracing::warn!(%error, "a credential cleanup wasn't recorded");
            }
        }
    }
}

fn saved(result: Result<(), VaultError>) -> SaveResult {
    match result {
        Ok(()) => SaveResult::Committed,
        Err(error) => SaveResult::NotCommitted(error),
    }
}

fn deleted(result: Result<(), VaultError>) -> DeleteResult {
    match result {
        Ok(()) => DeleteResult::Gone,
        Err(error) => DeleteResult::Refused(error),
    }
}

struct JsonBuffer {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
}

impl fmt::Debug for JsonBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JsonBuffer(***)")
    }
}

impl Write for JsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "record exceeds its byte bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode<T: Serialize>(value: &T, limit: usize) -> Result<SecretBytes, VaultError> {
    let mut buffer = JsonBuffer {
        bytes: Zeroizing::new(Vec::with_capacity(limit)),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).map_err(|_| VaultError::Bounds)?;
    Ok(Secret::new(buffer.bytes))
}

pub(super) fn decode<T: DeserializeOwned>(
    bytes: &SecretBytes,
    limit: usize,
) -> Result<T, VaultError> {
    if bytes.expose().len() > limit {
        return Err(VaultError::Bounds);
    }
    serde_json::from_slice(bytes.expose()).map_err(|_| VaultError::Unreadable)
}

fn valid_host(host: &str) -> bool {
    host.len() == 45
        && host.starts_with("urn:uuid:")
        && host.bytes().enumerate().skip(9).all(|(index, byte)| {
            if [17, 22, 27, 32].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
        && host.as_bytes()[23] == b'4'
        && b"89ab".contains(&host.as_bytes()[28])
}

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;
