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
pub(super) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

struct System;

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
    let mut bytes = Zeroizing::new(Vec::with_capacity(active.layout.bytes()));
    for index in 0..active.layout.chunks() {
        let found = store
            .get(&target(service, active.kind, active.generation, index))?
            .ok_or(AuthorityError::Unsettled)?;
        if found.expose().len() != active.layout.chunk_len(index) {
            return Err(AuthorityError::Unsettled);
        }
        bytes.extend_from_slice(found.expose());
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
