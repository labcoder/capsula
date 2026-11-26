//! Exact-name Credential Manager I/O for one write-once generation. Every name is written once,
//! so a name can only ever hold the bytes written there, or nothing.

use std::thread;
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::generation_target;
use zeroize::Zeroizing;

use super::{ActiveRef, AuthorityError, Generation, Kind, Layout, Observed, Retired, WritePlan};
use crate::platform::credential::{CHUNK_SIZE, CredentialStore, SecretBytes};

/// How long a save waits for its own fresh names to appear [guess]: about 16 times the 120 ms
/// miss OSC-233 measured. Waiting is safe because a write-once name can't hold anything else.
pub(crate) const SETTLE: Duration = Duration::from_secs(2);
/// The wait between reads of a name that isn't visible yet [guess].
const POLL: Duration = Duration::from_millis(10);

/// A Pending generation whose every chunk read back exactly; only [`write`] makes one.
#[derive(Debug)]
#[must_use = "a verified candidate stays Pending until it's activated"]
pub(crate) struct Verified(WritePlan);

impl Verified {
    pub(super) fn into_plan(self) -> WritePlan {
        self.0
    }
}

fn target(service: &str, kind: Kind, generation: Generation, index: usize) -> String {
    generation_target(service, kind.label(), &generation.hex(), index)
}

fn chunk(bytes: &[u8], layout: Layout, index: usize) -> Result<&[u8], AuthorityError> {
    let start = index * CHUNK_SIZE;
    bytes
        .get(start..start + layout.chunk_len(index))
        .ok_or(AuthorityError::Bounds)
}

/// Monotonic time and waiting, so the settle tests move time without sleeping.
pub(crate) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

pub(crate) struct System;

impl Clock for System {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Writes the plan's names once each, then reads each until it holds exactly what was written.
/// Absence is waited out for [`SETTLE`] in all; any other content fails at once.
///
/// # Errors
/// Bounds when the value doesn't fit the plan, Store for a refused write or read, and Unsettled
/// for a name not seen in time or holding other bytes.
pub(crate) fn write(
    store: &dyn CredentialStore,
    service: &str,
    plan: WritePlan,
    secret: &SecretBytes,
) -> Result<Verified, AuthorityError> {
    write_within(store, service, plan, secret, SETTLE, &System)
}

/// [`write`] with one `settle` budget across every chunk: a read seen at or after the deadline
/// doesn't count, and no wait runs past it.
pub(super) fn write_within(
    store: &dyn CredentialStore,
    service: &str,
    plan: WritePlan,
    secret: &SecretBytes,
    settle: Duration,
    clock: &dyn Clock,
) -> Result<Verified, AuthorityError> {
    let bytes = secret.expose();
    if bytes.len() != plan.layout.bytes() {
        return Err(AuthorityError::Bounds);
    }
    for index in 0..plan.layout.chunks() {
        let part = Secret::new(Zeroizing::new(chunk(bytes, plan.layout, index)?.to_vec()));
        store.set(&target(service, plan.kind, plan.generation, index), &part)?;
    }
    let deadline = clock.now() + settle;
    for index in 0..plan.layout.chunks() {
        let name = target(service, plan.kind, plan.generation, index);
        let expected = chunk(bytes, plan.layout, index)?;
        loop {
            let found = store.get(&name)?;
            if found
                .as_ref()
                .is_some_and(|found| found.expose().as_slice() != expected)
            {
                return Err(AuthorityError::Unsettled);
            }
            let now = clock.now();
            if now >= deadline {
                return Err(AuthorityError::Unsettled);
            }
            if found.is_some() {
                break;
            }
            clock.sleep(POLL.min(deadline - now));
        }
    }
    Ok(Verified(plan))
}

/// Reads the Active generation in one exact pass; there's no other name to fall back to.
///
/// # Errors
/// Unsettled for an absent chunk or one of the wrong length; Store for a refused read.
pub(crate) fn read(
    store: &dyn CredentialStore,
    service: &str,
    active: &ActiveRef,
) -> Result<SecretBytes, AuthorityError> {
    read_active(store, service, active).map_err(|error| match error {
        AuthorityError::MissingChunk | AuthorityError::WrongChunkLength => {
            AuthorityError::Unsettled
        }
        error => error,
    })
}

pub(crate) fn read_active(
    store: &dyn CredentialStore,
    service: &str,
    active: &ActiveRef,
) -> Result<SecretBytes, AuthorityError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(active.layout.bytes()));
    for index in 0..active.layout.chunks() {
        let found =
            read_chunk(store, service, active, index)?.ok_or(AuthorityError::MissingChunk)?;
        bytes.extend_from_slice(found.expose());
    }
    Ok(Secret::new(bytes))
}

pub(in crate::connections) fn has_missing_chunk(
    store: &dyn CredentialStore,
    service: &str,
    active: &ActiveRef,
) -> Result<bool, AuthorityError> {
    let mut missing = false;
    for index in 0..active.layout.chunks() {
        missing |= read_chunk(store, service, active, index)?.is_none();
    }
    Ok(missing)
}

fn read_chunk(
    store: &dyn CredentialStore,
    service: &str,
    active: &ActiveRef,
    index: usize,
) -> Result<Option<SecretBytes>, AuthorityError> {
    let found = store.get(&target(service, active.kind, active.generation, index))?;
    if found
        .as_ref()
        .is_some_and(|part| part.expose().len() != active.layout.chunk_len(index))
    {
        return Err(AuthorityError::WrongChunkLength);
    }
    Ok(found)
}

/// Reads staged chunks without authorizing them; the caller must still validate and CAS.
///
/// # Errors
/// Unsettled for a missing or wrong-length chunk, Store for a refused read.
pub(crate) fn read_pending(
    store: &dyn CredentialStore,
    service: &str,
    plan: &WritePlan,
    clock: &dyn Clock,
) -> Result<SecretBytes, AuthorityError> {
    read_pending_within(store, service, plan, SETTLE, clock)
}

pub(crate) fn delete_pending(
    store: &dyn CredentialStore,
    service: &str,
    kind: Kind,
    id: Generation,
    layout: Layout,
) {
    let mut failed = false;
    for index in 0..layout.chunks() {
        failed |= store.delete(&target(service, kind, id, index)).is_err();
    }
    if failed {
        tracing::warn!("a staged credential could not be deleted");
    }
}

pub(super) fn read_pending_within(
    store: &dyn CredentialStore,
    service: &str,
    plan: &WritePlan,
    settle: Duration,
    clock: &dyn Clock,
) -> Result<SecretBytes, AuthorityError> {
    let deadline = clock.now() + settle;
    let mut bytes = Zeroizing::new(Vec::with_capacity(plan.layout.bytes()));
    for index in 0..plan.layout.chunks() {
        let name = target(service, plan.kind, plan.generation, index);
        loop {
            let found = store.get(&name)?;
            let now = clock.now();
            if now >= deadline {
                return Err(AuthorityError::Unsettled);
            }
            if let Some(found) = found {
                if found.expose().len() != plan.layout.chunk_len(index) {
                    return Err(AuthorityError::Unsettled);
                }
                bytes.extend_from_slice(found.expose());
                break;
            }
            clock.sleep(POLL.min(deadline - now));
        }
    }
    Ok(Secret::new(bytes))
}

/// Deletes a retired generation's names, then reads each once.
pub(crate) fn sweep(store: &dyn CredentialStore, service: &str, retired: &Retired) -> Observed {
    let names: Vec<String> = (0..retired.layout.chunks())
        .map(|index| target(service, retired.kind, retired.generation, index))
        .collect();
    let mut deleted = true;
    for name in &names {
        deleted &= store.delete(name).is_ok();
    }
    if deleted && names.iter().all(|name| matches!(store.get(name), Ok(None))) {
        Observed::Absent
    } else {
        Observed::Remains
    }
}
