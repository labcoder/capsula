//! Locked, synchronous account records with a single-credential commit boundary.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::CREDENTIAL_SERVICE;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::error::VaultError;
use super::marker::{Disposition, MarkerUpdate, Phase, SignOutMarker};
use super::records::{Account, MAX_RECORD_BYTES, Record, RevocationList};
use crate::platform::credential::{
    CHUNK_SIZE, CredentialStore, MAX_CHUNKS, SecretBytes, delete_chunks, get_chunks, set_chunks,
};
use crate::platform::{self, SessionLock};
use lane_store::LaneStore;

/// A save's durable outcome, separate from cleanup after its manifest commit.
#[derive(Debug)]
#[must_use]
pub enum SaveResult {
    /// The new record is active; failed old-slot cleanup requires another sweep.
    Committed {
        /// Some old chunks could not be deleted.
        leftovers: bool,
    },
    /// The manifest never changed; the caller still owns the unsaved value.
    NotCommitted(VaultError),
}

/// Whether every name of a chunked record was deleted and checked absent.
#[derive(Debug)]
#[must_use]
pub enum DeleteResult {
    /// All 129 names are absent, not merely an absent manifest.
    Gone,
    /// A delete or by-name check failed; retry must not claim success yet.
    Refused(VaultError),
}

#[derive(Debug, Clone, Copy)]
enum Chunked {
    Account,
    Revoke,
}

impl Chunked {
    const fn name(self) -> &'static str {
        match self {
            Self::Account => "chatgpt",
            Self::Revoke => "revoke",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Account => 0,
            Self::Revoke => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    slot: usize,
    chunks: usize,
}

/// Owns the session lock while its store is accessible, including during blocking work.
///
/// Move the vault into blocking work, rather than dropping its guard when an async waiter ends.
#[must_use = "the vault must retain its session lock until all blocking work ends"]
pub struct Vault<S: CredentialStore> {
    store: LaneStore<S>,
    service: String,
    clean: [[bool; 2]; 2],
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
        use kumochi_protocol::constants::{ANTHROPIC_KEY_ACCOUNT, ANTHROPIC_STAGING_ACCOUNT};
        let active = format!("{ANTHROPIC_KEY_ACCOUNT}.{}", self.service);
        let staged = format!("{ANTHROPIC_STAGING_ACCOUNT}.{}", self.service);
        if matches!(op, KeyOp::Remove) {
            let first = self.store.delete(&active);
            let second = self.store.delete(&staged);
            first.and(second).map_err(VaultError::from)?;
            return Ok(None);
        }
        let read = self.store.get(&staged);
        let adopting = matches!(op, KeyOp::Adopt) || !matches!(read, Ok(None));
        if adopting {
            let result = (|| {
                let bytes = read.map_err(VaultError::from)?.ok_or(KeyError::Rejected(
                    kumochi_protocol::message::KeyRejection::Format,
                ))?;
                let key = AnthropicKey::parse(bytes).map_err(KeyError::Rejected)?;
                let bytes = Secret::new(Zeroizing::new(key.expose().as_str().as_bytes().to_vec()));
                self.store.set(&active, &bytes).map_err(VaultError::from)?;
                Ok(Some(key))
            })();
            let cleanup = self.store.delete(&staged).map_err(VaultError::from);
            cleanup?;
            return result;
        }
        self.store
            .get(&active)
            .map_err(VaultError::from)?
            .map(AnthropicKey::parse)
            .transpose()
            .map_err(KeyError::Rejected)
    }

    /// Takes account-session ownership without reading any credential yet.
    ///
    /// # Errors
    /// Returns typed Busy for contention, or a path/lock failure otherwise.
    pub(in crate::connections) fn open(store: S) -> Result<Self, VaultError> {
        let path = platform::session_lock_path().ok_or(VaultError::NoDataDir)?;
        Self::with_service(store, CREDENTIAL_SERVICE, &path)
    }

    pub(crate) fn with_service(store: S, service: &str, path: &Path) -> Result<Self, VaultError> {
        let session = SessionLock::try_acquire(path)?;
        Ok(Self {
            store: LaneStore::new(store),
            service: service.to_owned(),
            clean: [[false; 2]; 2],
            _session: session,
        })
    }

    /// Reads or creates the machine-local random `UUIDv4` host identifier.
    ///
    /// # Errors
    /// Reports unreadable data, unavailable randomness or any refused credential operation.
    pub fn host_id(&mut self) -> Result<Secret<Zeroizing<String>>, VaultError> {
        let target = self.target("host");
        if let Some(bytes) = self.store.get(&target)? {
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
        self.store.set(&target, &encode(&host, 128)?)?;
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
        self.load(Chunked::Account)
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
        self.load(Chunked::Account)
    }

    /// Replaces the account through its inactive slot and atomic manifest.
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
            self.save(Chunked::Account, &bytes)
        })();
        saved(result)
    }

    /// Reads revocation-only tokens without making them usable as account tokens.
    ///
    /// # Errors
    /// Reports refused reads and malformed, incomplete or oversized revocation records.
    pub fn load_revocations(&self) -> Result<Option<Secret<RevocationList>>, VaultError> {
        self.load(Chunked::Revoke)
    }

    /// Persists sweep state first, then commits the independent revocation record.
    pub fn save_revocations(&mut self, list: &Secret<RevocationList>) -> SaveResult {
        let result = (|| {
            list.expose().validate()?;
            let bytes = encode(list.expose(), MAX_RECORD_BYTES)?;
            if self
                .marker()?
                .sign_out
                .is_some_and(|sign_out| sign_out.phase == Phase::Deleting)
            {
                return Err(VaultError::Fenced);
            }
            self.update_marker(MarkerUpdate::BeginSweep)?;
            self.save(Chunked::Revoke, &bytes)
        })();
        saved(result)
    }

    /// Reads token-free control state; malformed or unknown layouts fail closed.
    ///
    /// # Errors
    /// Reports an OS read refusal. Absent state is empty, not a stored default marker.
    pub fn marker(&self) -> Result<SignOutMarker, VaultError> {
        let Some(bytes) = self.store.get(&self.target("signout"))? else {
            return Ok(SignOutMarker::default());
        };
        Ok(match decode::<SignOutMarker>(&bytes, 200) {
            Ok(marker) if marker.valid() => marker,
            _ => SignOutMarker::fail_closed(),
        })
    }

    /// Merges a change into the current persisted marker under the held session lock.
    ///
    /// `Checkpoint` takes evidence from the caller's saved revocation results, before any delete.
    /// Its decision is immutable once the persisted phase is Deleting.
    ///
    /// # Errors
    /// Reports incompatible phases, refused marker writes/deletes, or incomplete by-name checks.
    pub fn update_marker(&mut self, update: MarkerUpdate) -> Result<SignOutMarker, VaultError> {
        let current = self.marker()?;
        match update {
            MarkerUpdate::SignInCommitted => {
                if current.sign_out.is_some()
                    || self
                        .load::<Account>(Chunked::Account)?
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
                            .load::<Account>(Chunked::Account)?
                            .is_some_and(|account| account.expose().tokens.is_some())
                        {
                            return Err(VaultError::Fenced);
                        }
                    }
                    Some(_) => {}
                }
            }
            MarkerUpdate::FinishSweep if current.sign_out.is_none() => {
                self.assert_absent(Chunked::Revoke, None)?;
            }
            MarkerUpdate::FinishSignOut => self.assert_sign_out_clean(current)?,
            _ => {}
        }
        let next = current.merge(update)?;
        if next == current {
            return Ok(next);
        }
        if next == SignOutMarker::default() {
            self.store.delete(&self.target("signout"))?;
        } else {
            self.store
                .set(&self.target("signout"), &encode(&next, 200)?)?;
        }
        Ok(next)
    }

    /// Removes both account slots and its manifest only after a persisted deletion checkpoint.
    pub fn delete_account(&mut self) -> DeleteResult {
        let result = (|| {
            let marker = self.marker()?;
            if marker
                .sign_out
                .is_none_or(|sign_out| sign_out.phase != Phase::Deleting)
            {
                return Err(VaultError::Fenced);
            }
            self.erase(Chunked::Account)
        })();
        deleted(result)
    }

    /// Removes all revocation names while retaining the independent sweep/sign-out marker.
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
            self.erase(Chunked::Revoke)
        })();
        deleted(result)
    }

    /// Deletes both records after the checkpoint, driven by the marker alone: a record's names
    /// go whether or not its manifest survives. Keep retains a readable registration's live slot
    /// and nothing else; with no readable registration, the record is erased.
    pub fn delete_for_sign_out(&mut self) -> DeleteResult {
        let result = (|| {
            let sign_out = self
                .marker()?
                .sign_out
                .filter(|sign_out| sign_out.phase == Phase::Deleting)
                .ok_or(VaultError::Fenced)?;
            if sign_out.disposition == Disposition::Erase || !self.keep_registration()? {
                self.erase(Chunked::Account)?;
            }
            self.erase(Chunked::Revoke)
        })();
        deleted(result)
    }

    /// Rewrites the account to its registration, then deletes every other name of the record.
    /// False when there's no registration or its data is damaged, so the caller erases instead.
    fn keep_registration(&mut self) -> Result<bool, VaultError> {
        // A refused read is retried, never taken as a reason to erase the registration.
        let Some(account) = self.load::<Account>(Chunked::Account).or_else(|error| {
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
            self.save(Chunked::Account, &encode(&registration, MAX_RECORD_BYTES)?)?;
        }
        let live = self
            .manifest(Chunked::Account)?
            .ok_or(VaultError::Unreadable)?;
        let name = Chunked::Account.name();
        delete_chunks(
            &self.store,
            &self.target(&format!("{name}/{}", 1 - live.slot)),
            MAX_CHUNKS,
        )?;
        for index in live.chunks..MAX_CHUNKS {
            self.store
                .delete(&self.target(&format!("{name}/{}/{index}", live.slot)))?;
        }
        self.assert_absent(Chunked::Account, Some(live))?;
        Ok(true)
    }

    fn target(&self, entry: &str) -> String {
        format!("{}/{entry}", self.service)
    }

    fn manifest(&self, record: Chunked) -> Result<Option<Manifest>, VaultError> {
        let Some(bytes) = self.store.get(&self.target(record.name()))? else {
            return Ok(None);
        };
        let manifest: Manifest = decode(&bytes, 200)?;
        if manifest.slot > 1
            || !(1..=MAX_RECORD_BYTES.div_ceil(CHUNK_SIZE)).contains(&manifest.chunks)
        {
            return Err(VaultError::Unreadable);
        }
        Ok(Some(manifest))
    }

    fn load<T: Record + DeserializeOwned>(
        &self,
        record: Chunked,
    ) -> Result<Option<Secret<T>>, VaultError> {
        let Some(manifest) = self.manifest(record)? else {
            return Ok(None);
        };
        let bytes = get_chunks(
            &self.store,
            &self.target(&format!("{}/{}", record.name(), manifest.slot)),
            manifest.chunks,
        )?;
        let value: T = decode(&bytes, MAX_RECORD_BYTES)?;
        value.validate()?;
        Ok(Some(Secret::new(value)))
    }

    fn save(&mut self, record: Chunked, bytes: &SecretBytes) -> Result<bool, VaultError> {
        let old = self.manifest(record)?;
        let slot = old.map_or(0, |manifest| 1 - manifest.slot);
        let base = self.target(&format!("{}/{slot}", record.name()));
        if !self.clean[record.index()][slot] {
            delete_chunks(&self.store, &base, MAX_CHUNKS)?;
        }
        self.clean[record.index()][slot] = false;
        let chunks = set_chunks(&self.store, &base, bytes)?;
        self.store.set(
            &self.target(record.name()),
            &encode(&Manifest { slot, chunks }, 200)?,
        )?;
        let mut leftovers = false;
        if let Some(old) = old {
            let base = self.target(&format!("{}/{}", record.name(), old.slot));
            let clean = delete_chunks(&self.store, &base, old.chunks).is_ok();
            self.clean[record.index()][old.slot] = clean;
            leftovers = !clean;
        }
        Ok(leftovers)
    }

    fn erase(&mut self, record: Chunked) -> Result<(), VaultError> {
        self.clean[record.index()] = [false; 2];
        // The manifest goes first, so no interruption leaves it naming a deleted chunk.
        self.store.delete(&self.target(record.name()))?;
        let mut result = Ok(());
        for slot in 0..2 {
            let deleted = delete_chunks(
                &self.store,
                &self.target(&format!("{}/{slot}", record.name())),
                MAX_CHUNKS,
            )
            .map_err(VaultError::from);
            result = result.and(deleted);
        }
        result?;
        self.assert_absent(record, None)?;
        self.clean[record.index()] = [true; 2];
        Ok(())
    }

    fn assert_absent(&self, record: Chunked, keep: Option<Manifest>) -> Result<(), VaultError> {
        if keep.is_none() && self.store.get(&self.target(record.name()))?.is_some() {
            return Err(VaultError::NotGone);
        }
        for slot in 0..2 {
            for index in 0..MAX_CHUNKS {
                if keep.is_some_and(|keep| keep.slot == slot && index < keep.chunks) {
                    continue;
                }
                if self
                    .store
                    .get(&self.target(&format!("{}/{slot}/{index}", record.name())))?
                    .is_some()
                {
                    return Err(VaultError::NotGone);
                }
            }
        }
        Ok(())
    }

    fn assert_sign_out_clean(&self, marker: SignOutMarker) -> Result<(), VaultError> {
        let sign_out = marker.sign_out.ok_or(VaultError::Fenced)?;
        if sign_out.phase != Phase::Deleting {
            return Err(VaultError::Fenced);
        }
        self.assert_absent(Chunked::Revoke, None)?;
        let keep = match sign_out.disposition {
            Disposition::Erase => None,
            Disposition::Keep => {
                if let Some(account) = self.load::<Account>(Chunked::Account)? {
                    let account = account.expose();
                    if account.tokens.is_some() || account.email.is_some() || account.plan_enabled {
                        return Err(VaultError::Fenced);
                    }
                }
                self.manifest(Chunked::Account)?
            }
        };
        self.assert_absent(Chunked::Account, keep)
    }
}

fn saved(result: Result<bool, VaultError>) -> SaveResult {
    match result {
        Ok(leftovers) => SaveResult::Committed { leftovers },
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

fn encode<T: Serialize>(value: &T, limit: usize) -> Result<SecretBytes, VaultError> {
    let mut buffer = JsonBuffer {
        bytes: Zeroizing::new(Vec::with_capacity(limit)),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).map_err(|_| VaultError::Bounds)?;
    Ok(Secret::new(buffer.bytes))
}

fn decode<T: DeserializeOwned>(bytes: &SecretBytes, limit: usize) -> Result<T, VaultError> {
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
mod lane_store;
#[cfg(test)]
mod tests;
