//! The capture exclusion's lease, apart from the windows it covers: one lease at a time, a
//! receipt for each surface, and a cleanup that restores every surface or keeps the debt.
//!
//! The lease's decisions are a core driven by asks and answers: it says what to ask a surface
//! next and takes each answer whenever it comes, so its surfaces may live on other threads.
//! `acquire`, `attach`, `cleanup` and `expire` drive it through `Surfaces`, answering at once.

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
/// How long an acquire waits for every snapshot and exclude, counted from its start.
pub(super) const BOUND: Duration = Duration::from_millis(250);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
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
}

/// One question for one surface. Its answer quotes `id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
    pub id: AskId,
    pub slot: Slot,
    pub op: Op,
}

/// The ask's lease and its serial within that lease, so an answer counts only while it waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AskId {
    generation: LeaseGeneration,
    serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Read the surface's capture state and change nothing.
    Snapshot,
    /// Exclude the surface from capture, with its slot's receipt.
    Exclude,
    /// Put back the capture state its slot's receipt holds.
    Restore,
}

/// Where the lease stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stand {
    /// No lease.
    Idle,
    /// Snapshots or excludes are still out, and nothing is granted.
    Acquiring,
    /// Every bound surface's exclude is confirmed.
    Granted,
    /// Revoked, with restores still to ask or to answer.
    Restoring,
    /// Revoked, and a restore failed: it waits to be asked again, or for its surface to be gone.
    Debt,
}

/// Each carries the acquire's bound while the acquire is still out.
enum Stage {
    Snapshots(Instant),
    Excludes(Instant),
    Granted,
    Revoked,
}

/// What a slot holds. A wait's `None` is an ask not yet taken, `Some` the serial it went out as.
enum Bound<R> {
    Free,
    Snapshot { asked: Option<u64>, attached: bool },
    Held(R, Step),
}

enum Step {
    /// The snapshot is in; the excludes wait for every other snapshot.
    Read,
    Exclude(Option<u64>),
    Covered,
    Restore(Option<u64>),
    Debt,
}

impl<R> Bound<R> {
    fn wait(&mut self) -> Option<(Op, &mut Option<u64>)> {
        match self {
            Self::Snapshot { asked, .. } => Some((Op::Snapshot, asked)),
            Self::Held(_, Step::Exclude(asked)) => Some((Op::Exclude, asked)),
            Self::Held(_, Step::Restore(asked)) => Some((Op::Restore, asked)),
            Self::Free | Self::Held(_, Step::Read | Step::Covered | Step::Debt) => None,
        }
    }
}

pub struct ExclusionLease<R> {
    pub(super) key: Key,
    deadline: Instant,
    stage: Stage,
    serial: u64,
    slots: [Bound<R>; 8],
}

impl<R> ExclusionLease<R> {
    /// Once the excludes have begun, every receipt held is restored, asked or not, as cleanup
    /// always has; a restore already out keeps waiting. Before then nothing has changed.
    fn revoke(&mut self) {
        let excluding = !matches!(self.stage, Stage::Snapshots(_));
        self.stage = Stage::Revoked;
        for bound in &mut self.slots {
            *bound = match std::mem::replace(bound, Bound::Free) {
                Bound::Held(receipt, Step::Restore(Some(serial))) => {
                    Bound::Held(receipt, Step::Restore(Some(serial)))
                }
                Bound::Held(receipt, _) if excluding => Bound::Held(receipt, Step::Restore(None)),
                Bound::Free | Bound::Snapshot { .. } | Bound::Held(..) => Bound::Free,
            };
        }
    }
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
    /// Starts a lease and asks every slot for its snapshot. The lease's lifetime and the
    /// acquire's bound both start at `now`.
    pub fn begin(&mut self, session: u64, input: CaptureFor, now: Instant) -> Result<Key, Error> {
        if self.lease.is_some() {
            return Err(Error::Busy);
        }
        self.generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let deadline = now.checked_add(LIFETIME).ok_or(Error::Exhausted)?;
        let bound = now.checked_add(BOUND).ok_or(Error::Exhausted)?;
        let key = Key {
            session,
            input,
            generation: LeaseGeneration(self.generation),
        };
        self.lease = Some(ExclusionLease {
            key,
            deadline,
            stage: Stage::Snapshots(bound),
            serial: 0,
            slots: std::array::from_fn(|_| Bound::Snapshot {
                asked: None,
                attached: false,
            }),
        });
        Ok(key)
    }

    /// A surface opened in `slot` under the lease, to be snapshotted and excluded before it
    /// shows. A slot already bound revokes the lease; a revoked lease refuses as `Busy`.
    pub fn opened(&mut self, slot: Slot) -> Result<(), Error> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        if matches!(lease.stage, Stage::Revoked) {
            return Err(Error::Busy);
        }
        let bound = &mut lease.slots[slot as usize];
        if matches!(bound, Bound::Free) {
            *bound = Bound::Snapshot {
                asked: None,
                attached: true,
            };
            return Ok(());
        }
        self.revoke();
        Err(Error::Surface)
    }

    /// The next ask to send, marked as sent; `None` while the lease only waits.
    pub fn next_ask(&mut self) -> Option<Ask> {
        let lease = self.lease.as_mut()?;
        for (slot, bound) in SLOTS.into_iter().zip(&mut lease.slots) {
            if let Some((op, asked)) = bound.wait()
                && asked.is_none()
            {
                lease.serial += 1;
                *asked = Some(lease.serial);
                let id = AskId {
                    generation: lease.key.generation,
                    serial: lease.serial,
                };
                return Some(Ask { id, slot, op });
            }
        }
        None
    }

    /// The receipt `slot` holds, which its exclude and restore asks carry.
    pub fn receipt(&self, slot: Slot) -> Option<&R> {
        match &self.lease.as_ref()?.slots[slot as usize] {
            Bound::Held(receipt, _) => Some(receipt),
            Bound::Free | Bound::Snapshot { .. } => None,
        }
    }

    /// A snapshot's answer. A failure, or no surface in a slot opened under the lease, revokes it.
    pub fn snapshot(&mut self, id: AskId, read: Result<Option<R>, Error>) {
        let Some((lease, index)) = self.waiting(id, Op::Snapshot) else {
            return;
        };
        let attached = matches!(lease.slots[index], Bound::Snapshot { attached: true, .. });
        let step = if matches!(lease.stage, Stage::Snapshots(_)) {
            Step::Read
        } else {
            Step::Exclude(None)
        };
        match read {
            Ok(Some(receipt)) => lease.slots[index] = Bound::Held(receipt, step),
            Ok(None) if !attached => lease.slots[index] = Bound::Free,
            Ok(None) | Err(_) => lease.revoke(),
        }
        self.settle();
    }

    /// An exclude's answer. A failure revokes the lease.
    pub fn excluded(&mut self, id: AskId, done: Result<(), Error>) {
        let Some((lease, index)) = self.waiting(id, Op::Exclude) else {
            return;
        };
        if done.is_err() {
            lease.revoke();
        } else if let Bound::Held(_, step) = &mut lease.slots[index] {
            *step = Step::Covered;
        }
        self.settle();
    }

    /// A restore's answer, which counts whenever it comes. A failure stays as debt.
    pub fn restored(&mut self, id: AskId, done: Result<(), Error>) {
        let Some((lease, index)) = self.waiting(id, Op::Restore) else {
            return;
        };
        if done.is_ok() {
            lease.slots[index] = Bound::Free;
        } else if let Bound::Held(_, step) = &mut lease.slots[index] {
            *step = Step::Debt;
        }
        self.settle();
    }

    /// `slot`'s surface is proved gone, so whatever it waited for is settled without a restore.
    pub fn gone(&mut self, slot: Slot) {
        if let Some(lease) = &mut self.lease {
            lease.slots[slot as usize] = Bound::Free;
        }
        self.settle();
    }

    /// At or after the acquire's bound, every snapshot or exclude still out has failed.
    pub fn overdue(&mut self, now: Instant) {
        if let Some(lease) = &mut self.lease
            && let Stage::Snapshots(until) | Stage::Excludes(until) = lease.stage
            && now >= until
        {
            lease.revoke();
            self.settle();
        }
    }

    /// Starts the restores, or asks again for those that failed. Before any exclude, it ends the
    /// acquire with nothing changed.
    pub fn revoke(&mut self) {
        if let Some(lease) = &mut self.lease {
            lease.revoke();
        }
        self.settle();
    }

    /// Revokes `key`'s lease once `now` is at or after its deadline, and says whether it did.
    pub fn lapse(&mut self, key: Key, now: Instant) -> bool {
        let due = self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.key == key && now >= lease.deadline);
        if due {
            self.revoke();
        }
        due
    }

    pub fn stand(&self) -> Stand {
        let Some(lease) = &self.lease else {
            return Stand::Idle;
        };
        match lease.stage {
            Stage::Snapshots(_) | Stage::Excludes(_) => Stand::Acquiring,
            Stage::Granted => Stand::Granted,
            Stage::Revoked
                if lease
                    .slots
                    .iter()
                    .any(|bound| matches!(bound, Bound::Held(_, Step::Restore(_)))) =>
            {
                Stand::Restoring
            }
            Stage::Revoked => Stand::Debt,
        }
    }

    fn waiting(&mut self, id: AskId, op: Op) -> Option<(&mut ExclusionLease<R>, usize)> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| lease.key.generation == id.generation)?;
        let index = lease.slots.iter_mut().position(|bound| {
            bound
                .wait()
                .is_some_and(|(waits, asked)| waits == op && *asked == Some(id.serial))
        })?;
        Some((lease, index))
    }

    /// The excludes start once every snapshot is in, the grant comes once every exclude is
    /// confirmed, and a revoked lease ends once it holds nothing.
    fn settle(&mut self) {
        let Some(lease) = &mut self.lease else {
            return;
        };
        if let Stage::Snapshots(until) = lease.stage
            && !lease
                .slots
                .iter()
                .any(|bound| matches!(bound, Bound::Snapshot { .. }))
        {
            lease.stage = Stage::Excludes(until);
            for bound in &mut lease.slots {
                if let Bound::Held(_, step @ Step::Read) = bound {
                    *step = Step::Exclude(None);
                }
            }
        }
        if lease
            .slots
            .iter()
            .all(|bound| matches!(bound, Bound::Free | Bound::Held(_, Step::Covered)))
        {
            match lease.stage {
                Stage::Excludes(_) => lease.stage = Stage::Granted,
                Stage::Revoked => self.lease = None,
                Stage::Snapshots(_) | Stage::Granted => {}
            }
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
        let key = self.begin(session, input, now)?;
        let answered = self.answer_at_once(surfaces);
        if self.live(key.generation) {
            return Ok(key);
        }
        match answered {
            Err((Op::Snapshot, error)) => Err(error),
            _ => Err(Error::Surface),
        }
    }

    pub fn attach<S: Surfaces<Receipt = R>>(
        &mut self,
        slot: Slot,
        surfaces: &mut S,
    ) -> Result<(), Error> {
        let Some(generation) = self.lease.as_ref().map(|lease| lease.key.generation) else {
            return Ok(());
        };
        if self.live(generation)
            && let Some(prior) = self.receipt(slot)
        {
            if S::gone(prior) {
                self.gone(slot);
            } else if surfaces.same(slot, prior) {
                return Ok(());
            }
        }
        let opened = self.opened(slot);
        if opened == Err(Error::Busy) {
            return opened;
        }
        let answered = self.answer_at_once(surfaces).map_err(|(_, error)| error);
        opened.and(answered)?;
        if self.live(generation) {
            Ok(())
        } else {
            Err(Error::Surface)
        }
    }

    pub fn cleanup(&mut self, surfaces: &mut impl Surfaces<Receipt = R>) -> Result<(), Error> {
        self.revoke();
        self.answer_at_once(surfaces).map_err(|(_, error)| error)
    }

    pub fn expire(
        &mut self,
        key: Key,
        now: Instant,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<(), Error> {
        if self.lapse(key, now) {
            self.answer_at_once(surfaces).map_err(|(_, error)| error)
        } else {
            Ok(())
        }
    }

    /// Takes one ask at a time and answers it through `surfaces` before taking the next, so the
    /// surfaces see the calls in the order the synchronous machine always made them. Fails with
    /// the first snapshot or exclude that failed, or with `Surface` while restoration debt remains.
    fn answer_at_once(
        &mut self,
        surfaces: &mut impl Surfaces<Receipt = R>,
    ) -> Result<(), (Op, Error)> {
        let mut failed = Ok(());
        while let Some(Ask { id, slot, op }) = self.next_ask() {
            match op {
                Op::Snapshot => {
                    let read = surfaces.snapshot(slot);
                    if let Err(error) = &read {
                        failed = failed.and(Err((op, *error)));
                    }
                    self.snapshot(id, read);
                }
                Op::Exclude => {
                    let done = surfaces.exclude(slot, self.held(slot));
                    failed = failed.and(done.map_err(|error| (op, error)));
                    self.excluded(id, done);
                }
                Op::Restore => {
                    let done = surfaces.restore(slot, self.held(slot));
                    self.restored(id, done);
                }
            }
        }
        if self.stand() == Stand::Debt {
            return Err((Op::Restore, Error::Surface));
        }
        failed
    }

    fn held(&self, slot: Slot) -> &R {
        self.receipt(slot)
            .expect("an exclude or restore is asked only of a slot holding its receipt")
    }

    pub fn matches(&self, session: u64, input: CaptureFor) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key.session == session && lease.key.input == input)
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

    /// Whether `generation`'s lease still protects capture: granted and not revoked.
    pub fn live(&self, generation: LeaseGeneration) -> bool {
        self.lease.as_ref().is_some_and(|lease| {
            lease.key.generation == generation && matches!(lease.stage, Stage::Granted)
        })
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
        matches!(self.stage, Stage::Revoked)
    }

    pub fn holds(&self, slot: Slot) -> bool {
        matches!(self.slots[slot as usize], Bound::Held(..))
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
mod receipt_tests;
#[cfg(test)]
mod tests;
