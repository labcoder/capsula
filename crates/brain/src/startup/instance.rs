//! One instance item publication under session custody: publish, retire and settle (A-7 to
//! A-11). Each item call runs on its own blocking thread and is never abandoned.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kumochi_protocol::message::CAPABILITY_LEN;
use subtle::ConstantTimeEq as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zeroize::{Zeroize as _, Zeroizing};

use super::custody::SessionCustody;
use crate::platform::ShellProcess;
use crate::platform::shell_process::Verdict;
use crate::runtime::Stage;

type Key = Zeroizing<[u8; CAPABILITY_LEN]>;

/// The item calls a platform makes. Each blocks, so each runs on its own blocking thread.
pub(crate) trait InstanceStore: Send + Sync + 'static {
    /// A refusal kind, with no content.
    type Error: Copy + fmt::Debug + Send + 'static;
    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, Self::Error>;
    /// Ok only for an existing plain, protected item.
    fn inspect(&self) -> Result<(), Self::Error>;
    /// Replaces the existing item's value alone.
    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), Self::Error>;
    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), Self::Error>;
    fn delete(&self) -> Result<(), Self::Error>;
}

/// What an add found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Added {
    Fresh,
    Duplicate,
}

/// An item call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Add,
    Inspect,
    Update,
    Read,
    Delete,
}

/// Why publication or retirement refused. It carries no key bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refused<E> {
    #[error("the instance attempt was already used")]
    Used,
    #[error("the instance attempt's deadline passed")]
    Deadline,
    #[error("the parent was lost during the instance attempt")]
    Lost(Verdict),
    #[error("an instance item call failed at {0:?}")]
    Store(Step, E),
    #[error("an instance item call didn't report back")]
    Ambiguous,
    #[error("the instance item read back differs")]
    Mismatch,
    /// Refused after a write may have landed; `cleaned` when that item was deleted.
    #[error("the instance attempt was refused late (cleaned up: {cleaned})")]
    Late { cleaned: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Added(Added),
    Done,
    Matches(bool),
}

/// None when a forward call found the attempt fenced and made no call.
type Outcome<E> = Option<Result<Reply, E>>;

/// A key-sized buffer overwritten with zeros when dropped.
struct Wiped(Box<[u8; CAPABILITY_LEN]>);

impl Wiped {
    fn zeroed() -> Self {
        Self(Box::new([0; CAPABILITY_LEN]))
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        tests::wipes::record(&self.0[..]);
    }
}

/// The generated instance key. Publication and the responder each copy it, and it's wiped as
/// soon as both have, before READY.
pub(crate) struct OwnedKey(Box<Key>);

impl OwnedKey {
    /// A fresh random key.
    ///
    /// # Errors
    /// The system's random source failed.
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut key = Self(Box::new(Zeroizing::new([0; CAPABILITY_LEN])));
        getrandom::fill(&mut key.0[..])?;
        Ok(key)
    }

    pub(crate) fn key(&self) -> &Key {
        &self.0
    }
}

impl Drop for OwnedKey {
    fn drop(&mut self) {
        // Wiped here, not only by the inner `Zeroizing`, which drops after this body.
        self.0.zeroize();
        #[cfg(test)]
        tests::wipes::record(&self.0[..]);
    }
}

/// Each buffer an attempt made, for the tests: its address and the wipe log's length then.
#[cfg(test)]
type Made = Arc<std::sync::Mutex<Vec<(usize, usize)>>>;

enum Job {
    Add(Arc<Wiped>),
    Inspect,
    Update(Arc<Wiped>),
    Read(Arc<Wiped>),
    Delete,
}

impl Job {
    fn step(&self) -> Step {
        match self {
            Self::Add(_) => Step::Add,
            Self::Inspect => Step::Inspect,
            Self::Update(_) => Step::Update,
            Self::Read(_) => Step::Read,
            Self::Delete => Step::Delete,
        }
    }
}

/// What a forward call checks first, so one the pool starts late makes no item call.
struct Entry {
    shell: ShellProcess,
    deadline: Instant,
    invalid: Arc<AtomicBool>,
}

enum State<E> {
    Idle,
    /// Forward calls, then settlement's own; each call stays here until it's joined.
    InFlight {
        bytes: Arc<Wiped>,
        pending: Option<(Step, JoinHandle<Outcome<E>>)>,
        wrote: bool,
        /// A readback's verdict once one returned: true only for an exact match.
        matched: Option<bool>,
        invalid: Arc<AtomicBool>,
    },
    Published,
    /// The final delete, retirement's or settlement's cleanup, holding no key and kept until
    /// it's joined.
    Retiring(JoinHandle<Outcome<E>>),
    /// The first terminal result, which every later `settle` returns.
    Settled(Stage),
}

enum Stop<E> {
    Refused(Refused<E>),
    /// A deadline or a loss: only settlement runs after it.
    Invalid(Refused<E>),
}

/// Owns one publication and the custody it runs under, from its first call to its settlement.
pub(crate) struct InstanceAttempt<S: InstanceStore> {
    _custody: SessionCustody,
    store: Arc<S>,
    state: State<S::Error>,
    #[cfg(test)]
    made: Made,
}

impl<S: InstanceStore> fmt::Debug for InstanceAttempt<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            State::Idle => "Idle",
            State::InFlight { .. } => "InFlight",
            State::Published => "Published",
            State::Retiring(_) => "Retiring",
            State::Settled(_) => "Settled",
        };
        f.debug_struct("InstanceAttempt")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

impl<S: InstanceStore> InstanceAttempt<S> {
    pub(in crate::startup) fn new(custody: SessionCustody, store: Arc<S>) -> Self {
        Self {
            _custody: custody,
            store,
            state: State::Idle,
            #[cfg(test)]
            made: Arc::default(),
        }
    }

    /// Stores `key` as the item and reads it back exactly, once. A deadline or a parent loss
    /// refuses; a write already in flight is still awaited and settled.
    pub(crate) async fn publish(
        &mut self,
        key: &Key,
        deadline: Instant,
        shell: &ShellProcess,
    ) -> Result<(), Refused<S::Error>> {
        if !matches!(self.state, State::Idle) {
            return Err(Refused::Used);
        }
        let mut bytes = Wiped::zeroed();
        bytes.0.copy_from_slice(&key[..]);
        #[cfg(test)]
        tests::wipes::made(&self.made, &bytes);
        self.state = State::InFlight {
            bytes: Arc::new(bytes),
            pending: None,
            wrote: false,
            matched: None,
            invalid: Arc::default(),
        };
        match self.forward(deadline, shell).await {
            Ok(()) => {
                self.state = State::Published;
                Ok(())
            }
            Err(Stop::Refused(refused)) => {
                self.state = State::Settled(Stage::Confirmed);
                Err(refused)
            }
            Err(Stop::Invalid(reason)) => Err(self.settlement(reason).await),
        }
    }

    /// Deletes this owner's published item, once.
    pub(crate) async fn retire(&mut self) -> Result<(), Refused<S::Error>> {
        if !matches!(self.state, State::Published) {
            return Err(Refused::Used);
        }
        self.begin_retire();
        self.retired().await
    }

    /// Finishes whatever a dropped `publish` or `retire` left, cleans up what's ours, and
    /// returns the first terminal result from then on; custody must outlive this.
    pub(crate) async fn settle(&mut self) -> Stage {
        match self.state {
            State::Idle => self.state = State::Settled(Stage::Confirmed),
            State::Published => self.begin_retire(),
            State::InFlight { .. } => {
                self.settlement(Refused::Ambiguous).await;
            }
            State::Retiring(_) | State::Settled(_) => {}
        }
        if matches!(self.state, State::Retiring(_)) {
            return match self.retired().await {
                Ok(()) => Stage::Confirmed,
                Err(_) => Stage::Failed,
            };
        }
        match self.state {
            State::Settled(stage) => stage,
            _ => Stage::Failed,
        }
    }

    fn begin_retire(&mut self) {
        self.state = State::Settled(Stage::Failed); // Drops any key before the delete can start.
        self.state = State::Retiring(spawn(
            &self.store,
            Job::Delete,
            None,
            #[cfg(test)]
            &self.made,
        ));
    }

    /// Joins retirement's delete in place, so a dropped `retire` leaves it for `settle`.
    async fn retired(&mut self) -> Result<(), Refused<S::Error>> {
        let State::Retiring(handle) = &mut self.state else {
            return Err(Refused::Used);
        };
        let retired = match handle.await {
            Ok(Some(Ok(_))) => Ok(()),
            Ok(Some(Err(error))) => Err(Refused::Store(Step::Delete, error)),
            _ => Err(Refused::Ambiguous),
        };
        let stage = if retired.is_ok() {
            Stage::Confirmed
        } else {
            Stage::Failed
        };
        self.state = State::Settled(stage);
        retired
    }

    async fn forward(
        &mut self,
        deadline: Instant,
        shell: &ShellProcess,
    ) -> Result<(), Stop<S::Error>> {
        let mut loss = shell.loss();
        let bytes = match &self.state {
            State::InFlight { bytes, .. } => Arc::clone(bytes),
            _ => return Err(Stop::Refused(Refused::Used)),
        };
        let added = self
            .step(Job::Add(Arc::clone(&bytes)), deadline, shell, &mut loss)
            .await?;
        if added == Reply::Added(Added::Duplicate) {
            self.step(Job::Inspect, deadline, shell, &mut loss).await?;
            self.step(Job::Update(Arc::clone(&bytes)), deadline, shell, &mut loss)
                .await?;
        }
        match self
            .step(Job::Read(bytes), deadline, shell, &mut loss)
            .await?
        {
            Reply::Matches(true) => Ok(()),
            _ => Err(Stop::Refused(Refused::Mismatch)),
        }
    }

    /// Fenced before dispatch and on return; while it runs, loss and the deadline race it, and
    /// a verdict already published beats a finished call.
    async fn step(
        &mut self,
        job: Job,
        deadline: Instant,
        shell: &ShellProcess,
        loss: &mut Option<watch::Receiver<Verdict>>,
    ) -> Result<Reply, Stop<S::Error>> {
        fenced(deadline, shell)?;
        let step = job.step();
        let State::InFlight {
            pending,
            wrote,
            matched,
            invalid,
            ..
        } = &mut self.state
        else {
            return Err(Stop::Refused(Refused::Used));
        };
        let entry = step_entry(step, shell, deadline, invalid);
        let handle = spawn(
            &self.store,
            job,
            entry,
            #[cfg(test)]
            &self.made,
        );
        let handle = &mut pending.insert((step, handle)).1;
        let raced = tokio::select! {
            biased;
            verdict = Verdict::lost(loss) => Err(Refused::Lost(verdict)),
            () = tokio::time::sleep_until(deadline.into()) => Err(Refused::Deadline),
            joined = handle => Ok(joined),
        };
        let joined = match raced {
            Ok(joined) => joined,
            Err(reason) => {
                invalid.store(true, Ordering::Release);
                return Err(Stop::Invalid(reason));
            }
        };
        *pending = None;
        let reply = match joined {
            Ok(Some(Ok(reply))) => Some(reply),
            Ok(None) => None,
            Ok(Some(Err(error))) => return Err(Stop::Refused(Refused::Store(step, error))),
            Err(_) => return Err(Stop::Refused(Refused::Ambiguous)),
        };
        *wrote |= matches!(
            (step, reply),
            (Step::Add, Some(Reply::Added(Added::Fresh))) | (Step::Update, Some(Reply::Done))
        );
        if step == Step::Read {
            *matched = Some(reply == Some(Reply::Matches(true)));
        }
        if let Err(stop) = fenced(deadline, shell) {
            invalid.store(true, Ordering::Release);
            return Err(stop);
        }
        reply.ok_or(Stop::Refused(Refused::Ambiguous))
    }

    /// After a deadline, a loss or a dropped `publish`: joins each call in place, and deletes
    /// only an acknowledged write whose one readback matched this attempt's bytes.
    async fn settlement(&mut self, reason: Refused<S::Error>) -> Refused<S::Error> {
        let refused = loop {
            let State::InFlight {
                bytes,
                pending,
                wrote,
                matched,
                invalid,
            } = &mut self.state
            else {
                return reason;
            };
            invalid.store(true, Ordering::Release);
            if let Some((step, handle)) = pending {
                let joined = handle.await;
                let step = *step;
                *pending = None;
                match (step, joined) {
                    (Step::Add, Ok(Some(Ok(Reply::Added(Added::Fresh)))))
                    | (Step::Update, Ok(Some(Ok(Reply::Done)))) => *wrote = true,
                    (Step::Add | Step::Update, Err(_)) => break Refused::Late { cleaned: false },
                    (Step::Read, read) => {
                        *matched = Some(matches!(read, Ok(Some(Ok(Reply::Matches(true))))));
                    }
                    _ => {}
                }
                continue;
            }
            let job = match (*wrote, *matched) {
                (false, _) => break reason,
                (true, None) => Job::Read(Arc::clone(bytes)),
                (true, Some(true)) => {
                    self.begin_retire();
                    let cleaned = self.retired().await.is_ok();
                    return Refused::Late { cleaned };
                }
                (true, Some(false)) => break Refused::Late { cleaned: false },
            };
            let step = job.step();
            *pending = Some((
                step,
                spawn(
                    &self.store,
                    job,
                    None,
                    #[cfg(test)]
                    &self.made,
                ),
            ));
        };
        let stage = if matches!(refused, Refused::Late { cleaned: false }) {
            Stage::Failed
        } else {
            Stage::Confirmed
        };
        self.state = State::Settled(stage);
        refused
    }
}

fn step_entry(
    step: Step,
    shell: &ShellProcess,
    deadline: Instant,
    invalid: &Arc<AtomicBool>,
) -> Option<Entry> {
    matches!(step, Step::Add | Step::Inspect | Step::Update).then(|| Entry {
        shell: shell.clone(),
        deadline,
        invalid: Arc::clone(invalid),
    })
}

fn fenced<E>(deadline: Instant, shell: &ShellProcess) -> Result<(), Stop<E>> {
    shell
        .fence()
        .map_err(|verdict| Stop::Invalid(Refused::Lost(verdict)))?;
    if Instant::now() >= deadline {
        return Err(Stop::Invalid(Refused::Deadline));
    }
    Ok(())
}

fn spawn<S: InstanceStore>(
    store: &Arc<S>,
    job: Job,
    entry: Option<Entry>,
    #[cfg(test)] made: &Made,
) -> JoinHandle<Outcome<S::Error>> {
    let store = Arc::clone(store);
    #[cfg(test)]
    let made = Arc::clone(made);
    tokio::task::spawn_blocking(move || {
        if let Some(entry) = entry
            && (Instant::now() >= entry.deadline
                || entry.shell.fence().is_err()
                || entry.invalid.load(Ordering::Acquire))
        {
            return None;
        }
        Some(match job {
            Job::Add(key) => store.add(&key.0).map(Reply::Added),
            Job::Inspect => store.inspect().map(|()| Reply::Done),
            Job::Update(key) => store.update(&key.0).map(|()| Reply::Done),
            Job::Read(key) => {
                let mut copy = Wiped::zeroed();
                #[cfg(test)]
                tests::wipes::made(&made, &copy);
                store
                    .read(&mut copy.0)
                    .map(|()| Reply::Matches(copy.0[..].ct_eq(&key.0[..]).into()))
            }
            Job::Delete => store.delete().map(|()| Reply::Done),
        })
    })
}

#[cfg(test)]
mod tests;
