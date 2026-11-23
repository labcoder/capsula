//! The capture exclusion's lease, apart from the windows it covers: one lease at a time, a
//! receipt for each surface, and a cleanup that restores every surface or keeps the debt.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{CaptureFor, LeaseGeneration};

#[cfg(any(test, feature = "test-support"))]
#[expect(
    clippy::missing_assert_message,
    clippy::new_without_default,
    clippy::unwrap_used,
    reason = "the fixture's asserts and unwraps are test assertions, built outside cfg(test) for the shell's tests"
)]
pub mod fixture;

pub(super) const LIFETIME: Duration = Duration::from_secs(3);
const SLOTS: [Slot; 8] = [
    Slot::Dock,
    Slot::Proxy,
    Slot::Status,
    Slot::Text,
    Slot::Threads,
    Slot::Settings,
    Slot::Drawing,
    Slot::Onboarding,
];

#[derive(Clone, Copy, Debug)]
pub enum Slot {
    Dock,
    Proxy,
    Status,
    Text,
    Threads,
    Settings,
    Drawing,
    Onboarding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub(super) session: u64,
    pub(super) input: CaptureFor,
    pub(super) generation: LeaseGeneration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("exclusion is busy or has restoration debt")]
    Busy,
    #[error("exclusion generation or deadline exhausted")]
    Exhausted,
    #[error("a presence surface could not preserve its affinity")]
    Surface,
}

pub trait Surfaces {
    type Receipt;
    fn snapshot(&mut self, slot: Slot) -> Result<Option<Self::Receipt>, Error>;
    fn exclude(&mut self, slot: Slot, receipt: &Self::Receipt) -> Result<(), Error>;
    fn restore(&mut self, slot: Slot, receipt: &Self::Receipt) -> Result<(), Error>;
    fn gone(receipt: &Self::Receipt) -> bool;
    fn same(&self, slot: Slot, receipt: &Self::Receipt) -> bool;

    /// Attest continuous protection of the slot, or its absence, since acquisition.
    /// An adapter that cannot establish this must never provide positive typed proof.
    fn confirms_exclusion(&self, _slot: Slot, _receipt: Option<&Self::Receipt>) -> bool {
        false
    }
}

pub struct ExclusionLease<R> {
    pub(super) key: Key,
    deadline: Instant,
    revoked: bool,
    pub(super) receipts: [Option<R>; 8],
}

pub struct Exclusion<R> {
    pub(super) generation: u32,
    lease: Option<ExclusionLease<R>>,
}

impl<R> Default for Exclusion<R> {
    fn default() -> Self {
        Self {
            generation: 0,
            lease: None,
        }
    }
}

impl<R> Exclusion<R> {
    pub fn acquire(
        &mut self,
        session: u64,
        input: CaptureFor,
        now: Instant,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<Key, Error> {
        if self.lease.is_some() {
            return Err(Error::Busy);
        }
        self.generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let deadline = now.checked_add(LIFETIME).ok_or(Error::Exhausted)?;
        let key = Key {
            session,
            input,
            generation: LeaseGeneration(self.generation),
        };
        let mut receipts = std::array::from_fn(|_| None);
        for slot in SLOTS {
            receipts[slot as usize] = surfaces.snapshot(slot)?;
        }
        self.lease = Some(ExclusionLease {
            key,
            deadline,
            revoked: false,
            receipts,
        });
        for slot in SLOTS {
            let receipt = &self.lease.as_ref().expect("installed above").receipts[slot as usize];
            if let Some(receipt) = receipt
                && surfaces.exclude(slot, receipt).is_err()
            {
                self.cleanup(surfaces)?;
                return Err(Error::Surface);
            }
        }
        Ok(key)
    }

    pub fn attach<S: Surfaces<Receipt = R>>(
        &mut self,
        slot: Slot,
        surfaces: &mut S,
    ) -> Result<(), Error> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        if lease.revoked {
            return Err(Error::Busy);
        }
        let receipt = &mut lease.receipts[slot as usize];
        if let Some(prior) = receipt.as_ref().filter(|prior| !S::gone(prior)) {
            if surfaces.same(slot, prior) {
                return Ok(());
            }
            self.cleanup(surfaces)?;
            return Err(Error::Surface);
        }
        let result = surfaces.snapshot(slot).and_then(|prior| {
            *receipt = prior;
            let prior = receipt.as_ref().ok_or(Error::Surface)?;
            surfaces.exclude(slot, prior)
        });
        if result.is_err() {
            self.cleanup(surfaces)?;
        }
        result
    }

    pub fn cleanup(&mut self, surfaces: &mut impl Surfaces<Receipt = R>) -> Result<(), Error> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        lease.revoked = true;
        for slot in SLOTS {
            let receipt = &mut lease.receipts[slot as usize];
            if let Some(prior) = receipt
                && surfaces.restore(slot, prior).is_ok()
            {
                *receipt = None;
            }
        }
        if lease.receipts.iter().any(Option::is_some) {
            return Err(Error::Surface);
        }
        self.lease = None;
        Ok(())
    }

    pub fn expire(
        &mut self,
        key: Key,
        now: Instant,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<(), Error> {
        if self.expired(key, now) {
            self.cleanup(surfaces)
        } else {
            Ok(())
        }
    }

    pub(crate) fn expired(&self, key: Key, now: Instant) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key == key && now >= lease.deadline)
    }

    pub(crate) fn key(&self, session: u64, input: CaptureFor) -> Option<Key> {
        self.lease
            .as_ref()
            .map(|lease| lease.key)
            .filter(|key| key.session == session && key.input == input)
    }

    /// Restore only this exact lease, leaving a newer owner untouched.
    ///
    /// # Errors
    /// Returns the restoration debt if a surface could not be restored.
    pub fn restore_matching(
        &mut self,
        key: Key,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<(), Error> {
        if self.lease.as_ref().is_some_and(|lease| lease.key == key) {
            self.cleanup(surfaces)?;
        }
        Ok(())
    }

    /// Check the exact lease and all current surface receipts before restoring.
    pub fn confirms(&self, key: Key, now: Instant, surfaces: &impl Surfaces<Receipt = R>) -> bool {
        self.lease.as_ref().is_some_and(|lease| {
            lease.key == key
                && !lease.revoked
                && now < lease.deadline
                && SLOTS.into_iter().all(|slot| {
                    surfaces.confirms_exclusion(slot, lease.receipts[slot as usize].as_ref())
                })
        })
    }

    pub fn matches(&self, session: u64, input: CaptureFor) -> bool {
        self.key(session, input).is_some()
    }

    pub fn owns(&self, generation: LeaseGeneration) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key.generation == generation)
    }

    /// When the lease in force ends, for its owner's timer.
    pub fn deadline(&self) -> Option<Instant> {
        self.lease.as_ref().map(|lease| lease.deadline)
    }

    /// Whether `generation`'s lease still protects capture: in force and not revoked.
    pub fn live(&self, generation: LeaseGeneration) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key.generation == generation && !lease.revoked)
    }
}

impl Key {
    pub fn generation(self) -> LeaseGeneration {
        self.generation
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Key {
    pub fn input(self) -> CaptureFor {
        self.input
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<R> ExclusionLease<R> {
    pub fn key(&self) -> Key {
        self.key
    }

    pub fn revoked(&self) -> bool {
        self.revoked
    }

    pub fn holds(&self, slot: Slot) -> bool {
        self.receipts[slot as usize].is_some()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<R> Exclusion<R> {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn lease(&self) -> Option<&ExclusionLease<R>> {
        self.lease.as_ref()
    }
}

#[cfg(test)]
mod tests;
