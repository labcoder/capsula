//! One instance item publication under session custody: publish, retire and settle (A-7 to
//! A-11). Each item call runs on its own blocking thread and is never abandoned.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kumochi_protocol::message::CAPABILITY_LEN;
use subtle::ConstantTimeEq as _;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};
use zeroize::Zeroizing;

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

enum Job {
    Add(Arc<Key>),
    Inspect,
    Update(Arc<Key>),
    Read(Arc<Key>),
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
    InFlight {
        bytes: Arc<Key>,
        pending: Option<(Step, JoinHandle<Outcome<E>>)>,
        wrote: bool,
        invalid: Arc<AtomicBool>,
    },
    Published,
    Settled,
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
    copies: Arc<std::sync::Mutex<Vec<std::sync::Weak<Key>>>>,
}

impl<S: InstanceStore> fmt::Debug for InstanceAttempt<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            State::Idle => "Idle",
            State::InFlight { .. } => "InFlight",
            State::Published => "Published",
            State::Settled => "Settled",
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
            copies: Arc::default(),
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
        let bytes = Arc::new(Zeroizing::new(**key));
        #[cfg(test)]
        self.copies
            .lock()
            .expect("the copy list")
            .push(Arc::downgrade(&bytes));
        self.state = State::InFlight {
            bytes,
            pending: None,
            wrote: false,
            invalid: Arc::default(),
        };
        match self.forward(deadline, shell).await {
            Ok(()) => {
                self.state = State::Published;
                Ok(())
            }
            Err(Stop::Refused(refused)) => {
                self.state = State::Settled;
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
        self.state = State::Settled;
        match self.run(Job::Delete).await {
            Ok(Some(Ok(_))) => Ok(()),
            Ok(Some(Err(error))) => Err(Refused::Store(Step::Delete, error)),
            _ => Err(Refused::Ambiguous),
        }
    }

    /// Awaits any call a dropped `publish` left in flight and cleans up what's ours; custody must
    /// outlive this.
    pub(crate) async fn settle(&mut self) -> Stage {
        match self.state {
            State::Idle | State::Settled => {
                self.state = State::Settled;
                Stage::Confirmed
            }
            State::Published => match self.retire().await {
                Ok(()) => Stage::Confirmed,
                Err(_) => Stage::Failed,
            },
            State::InFlight { .. } => match self.settlement(Refused::Ambiguous).await {
                Refused::Late { cleaned: false } => Stage::Failed,
                _ => Stage::Confirmed,
            },
        }
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
            &self.copies,
        );
        let handle = &mut pending.insert((step, handle)).1;
        let raced = tokio::select! {
            biased;
            verdict = lost(loss) => Err(Refused::Lost(verdict)),
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
        if let Err(stop) = fenced(deadline, shell) {
            invalid.store(true, Ordering::Release);
            return Err(stop);
        }
        reply.ok_or(Stop::Refused(Refused::Ambiguous))
    }

    /// After a deadline or loss: awaits the call in flight, and deletes only an acknowledged
    /// write whose readback matches this attempt's bytes.
    async fn settlement(&mut self, reason: Refused<S::Error>) -> Refused<S::Error> {
        let State::InFlight {
            bytes,
            pending,
            mut wrote,
            invalid,
        } = std::mem::replace(&mut self.state, State::Settled)
        else {
            return reason;
        };
        invalid.store(true, Ordering::Release);
        if let Some((step, handle)) = pending {
            match (step, handle.await) {
                (Step::Add, Ok(Some(Ok(Reply::Added(Added::Fresh)))))
                | (Step::Update, Ok(Some(Ok(Reply::Done)))) => wrote = true,
                (Step::Add | Step::Update, Err(_)) => return Refused::Late { cleaned: false },
                _ => {}
            }
        }
        if !wrote {
            return reason;
        }
        let matched = self.run(Job::Read(bytes)).await;
        let cleaned = matches!(matched, Ok(Some(Ok(Reply::Matches(true)))))
            && matches!(self.run(Job::Delete).await, Ok(Some(Ok(_))));
        Refused::Late { cleaned }
    }

    /// A settlement call, which runs whatever the fences say.
    async fn run(&self, job: Job) -> Result<Outcome<S::Error>, JoinError> {
        spawn(
            &self.store,
            job,
            None,
            #[cfg(test)]
            &self.copies,
        )
        .await
    }

    /// The key copies this attempt made that are still alive; Zeroizing wipes each as it drops.
    #[cfg(test)]
    pub(crate) fn live_copies(&self) -> usize {
        self.copies
            .lock()
            .expect("the copy list")
            .iter()
            .filter(|copy| copy.strong_count() > 0)
            .count()
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

async fn lost(loss: &mut Option<watch::Receiver<Verdict>>) -> Verdict {
    match loss {
        Some(loss) => Verdict::wait(loss).await,
        None => std::future::pending().await,
    }
}

fn spawn<S: InstanceStore>(
    store: &Arc<S>,
    job: Job,
    entry: Option<Entry>,
    #[cfg(test)] copies: &Arc<std::sync::Mutex<Vec<std::sync::Weak<Key>>>>,
) -> JoinHandle<Outcome<S::Error>> {
    let store = Arc::clone(store);
    #[cfg(test)]
    let copies = Arc::clone(copies);
    tokio::task::spawn_blocking(move || {
        if let Some(entry) = entry
            && (Instant::now() >= entry.deadline
                || entry.shell.fence().is_err()
                || entry.invalid.load(Ordering::Acquire))
        {
            return None;
        }
        Some(match job {
            Job::Add(key) => store.add(&key).map(Reply::Added),
            Job::Inspect => store.inspect().map(|()| Reply::Done),
            Job::Update(key) => store.update(&key).map(|()| Reply::Done),
            Job::Read(key) => {
                // One owner the tests can watch; Zeroizing wipes it when this call ends.
                let mut copy = Arc::new(Zeroizing::new([0; CAPABILITY_LEN]));
                let read = store.read(Arc::get_mut(&mut copy).expect("a fresh copy has one owner"));
                #[cfg(test)]
                copies
                    .lock()
                    .expect("the copy list")
                    .push(Arc::downgrade(&copy));
                read.map(|()| Reply::Matches(copy[..].ct_eq(&key[..]).into()))
            }
            Job::Delete => store.delete().map(|()| Reply::Done),
        })
    })
}

#[cfg(test)]
mod tests;
