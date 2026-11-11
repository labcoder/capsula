//! The page reader and the checkpoint helper: two threads beside the store's, each on its own side
//! connection, so a long read never holds up a write and a commit never copies the WAL itself.
//!
//! A read waits until the store's thread has finished every op accepted before it, so it sees
//! them; later ops may show too. The helper checkpoints after every commit and every read.

use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kumochi_protocol::message::DataToShell;
use tokio::sync::mpsc::UnboundedSender;

use crate::coordinator::{Input, StoreOp};
use crate::store::{Side, StoreError};

/// The helper's wait after a checkpoint that left frames behind, doubling up to `LAST_RETRY`
/// [guess].
const FIRST_RETRY: Duration = Duration::from_millis(50);
const LAST_RETRY: Duration = Duration::from_secs(1);

/// A page read, a context build or a title job's read, stamped with how many other ops were
/// accepted before it.
#[derive(Debug)]
pub(super) struct Read {
    pub(super) stamp: u64,
    /// A page/card read, context build, compaction or title-job read.
    pub(super) op: StoreOp,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct Progress {
    completed: u64,
    terminal: bool,
}

/// How many accepted ops the store's thread has finished, and whether it has ended.
#[derive(Debug, Default)]
pub(super) struct Barrier {
    progress: Mutex<Progress>,
    moved: Condvar,
    /// Hears that a read waits for an op.
    #[cfg(test)]
    pub(super) waiting: Option<UnboundedSender<super::super::Seen>>,
}

impl Barrier {
    pub(super) fn new(#[cfg(test)] waiting: Option<UnboundedSender<super::super::Seen>>) -> Self {
        Self {
            #[cfg(test)]
            waiting,
            ..Self::default()
        }
    }

    /// One more accepted op finished, whether it succeeded or not.
    pub(super) fn complete(&self) {
        lock(&self.progress).completed += 1;
        self.moved.notify_all();
    }

    fn end(&self) {
        lock(&self.progress).terminal = true;
        self.moved.notify_all();
    }

    /// Waits until `stamp` ops have finished; false when the store's thread ended first.
    #[cfg(test)]
    pub(super) fn wait(&self, stamp: u64) -> bool {
        let progress = lock(&self.progress);
        #[cfg(test)]
        if progress.completed < stamp
            && !progress.terminal
            && let Some(waiting) = &self.waiting
        {
            waiting.send(super::super::Seen::ReadWaiting).ok();
        }
        let progress = self
            .moved
            .wait_while(progress, |progress| {
                progress.completed < stamp && !progress.terminal
            })
            .unwrap_or_else(PoisonError::into_inner);
        !progress.terminal
    }

    fn wait_until(
        &self,
        stamp: u64,
        control: &crate::recall::control::Control,
    ) -> Result<(), crate::recall::Error> {
        #[cfg(test)]
        let mut notified = false;
        loop {
            control.check()?;
            let progress = lock(&self.progress);
            if progress.terminal {
                return Err(crate::recall::Error::Ended);
            }
            if progress.completed >= stamp {
                return Ok(());
            }
            #[cfg(test)]
            if !notified && let Some(waiting) = &self.waiting {
                notified = true;
                if waiting.send(super::super::Seen::ReadWaiting).is_err() {
                    tracing::debug!("a reader wait had no observer");
                }
            }
            drop(
                self.moved
                    .wait_timeout(progress, crate::recall::control::POLL)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }
}

#[derive(Debug, Default)]
struct Wanted {
    raised: bool,
    stopped: bool,
}

/// Asks the helper for a checkpoint; raises while one runs make one more.
#[derive(Debug, Default)]
pub(super) struct Checkpoints {
    wanted: Mutex<Wanted>,
    wake: Condvar,
}

impl Checkpoints {
    pub(super) fn raise(&self) {
        lock(&self.wanted).raised = true;
        self.wake.notify_all();
    }

    fn stop(&self) {
        lock(&self.wanted).stopped = true;
        self.wake.notify_all();
    }

    /// Waits for a raise, or for `retry` while work is owed, and takes the raise; false once
    /// stopped.
    fn next(&self, retry: Option<Duration>) -> bool {
        let quiet = |wanted: &mut Wanted| !wanted.raised && !wanted.stopped;
        let wanted = lock(&self.wanted);
        let mut wanted = match retry {
            None => self
                .wake
                .wait_while(wanted, quiet)
                .unwrap_or_else(PoisonError::into_inner),
            Some(retry) => {
                self.wake
                    .wait_timeout_while(wanted, retry, quiet)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
        wanted.raised = false;
        !wanted.stopped
    }
}

/// Ends the side when the store's thread ends, by a panic too: the side connections close for
/// good, so nothing opens the file again, waiting reads fail, and the helper stops.
#[derive(Debug)]
pub(super) struct Exit {
    pub(super) side: Arc<Side>,
    pub(super) barrier: Arc<Barrier>,
    pub(super) checkpoints: Arc<Checkpoints>,
}

impl Drop for Exit {
    fn drop(&mut self) {
        self.side.close();
        self.barrier.end();
        self.checkpoints.stop();
    }
}

/// Test hooks: who hears a read start and hold, and gates that hold one with a snapshot open.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct Hooks {
    pub(super) observed: Option<UnboundedSender<super::super::Seen>>,
    pub(super) gates: Option<mpsc::Receiver<super::Gate>>,
}

/// Answers each read in order, then ends once the reads' sender is gone.
#[expect(
    clippy::too_many_lines,
    reason = "one reader loop owns snapshot and terminal ordering"
)]
pub(super) fn spawn_reader(
    reads: mpsc::Receiver<Read>,
    exit: &Exit,
    acks: UnboundedSender<Input>,
    #[cfg(test)] hooks: Hooks,
) -> io::Result<JoinHandle<()>> {
    let (side, barrier, checkpoints) = (
        Arc::clone(&exit.side),
        Arc::clone(&exit.barrier),
        Arc::clone(&exit.checkpoints),
    );
    thread::Builder::new()
        .name("pages".to_owned())
        .spawn(move || {
            #[cfg(test)]
            let mut gates = Vec::new();
            for Read { stamp, op } in reads {
                #[cfg(test)]
                let gate = {
                    if let Some(observed) = &hooks.observed {
                        observed
                            .send(super::super::Seen::StoreOpStarted(op.clone()))
                            .ok();
                    }
                    gates.extend(hooks.gates.iter().flat_map(mpsc::Receiver::try_iter));
                    let at = gates
                        .iter()
                        .position(|gate: &super::Gate| (gate.matches)(&op));
                    at.map(|at| gates.remove(at))
                };
                let control = match &op {
                    StoreOp::ReadCall(call) => &call.control,
                    StoreOp::BuildContext(read) => &read.control,
                    StoreOp::CompactContext(read) => &read.control,
                    StoreOp::ReadJob(read) => &read.control,
                    StoreOp::ReadPage { key, .. } => &key.control,
                    StoreOp::ReadCards { cards } => &cards.key.control,
                    _ => unreachable!("only reads reach the reader"),
                };
                let admitted = barrier.wait_until(stamp, control);
                let open = admitted.is_ok();
                let reply = match op {
                    StoreOp::ReadCall(call) => Input::ReadFinished(context::read_call(
                        admitted.map(|()| &*side),
                        call,
                        #[cfg(test)]
                        || wait_at(gate, hooks.observed.as_ref()),
                    )),
                    StoreOp::ReadCards { cards } => {
                        let reply = if open {
                            side.read_until(&cards.key.control, |conn, control| {
                                control
                                    .with_handler(conn, |conn, scope| {
                                        scope
                                            .query(conn, || {
                                                #[cfg(test)]
                                                if let Some(gate) = gate {
                                                    hold(conn, &gate, hooks.observed.as_ref())?;
                                                }
                                                let read =
                                                    crate::store::context::Snapshot::begin(conn)?;
                                                let reply = read.cards(&cards)?;
                                                read.end()?;
                                                Ok(reply)
                                            })
                                            .map_err(StoreError::from)
                                    })
                                    .map_err(StoreError::from)?
                            })
                        } else {
                            Err(StoreError::Closed)
                        };
                        Input::CardsRead {
                            cards,
                            reply: reply.map_err(|_| ()),
                        }
                    }
                    StoreOp::BuildContext(build) => Input::ContextBuilt(context::read(
                        open.then_some(&*side),
                        build,
                        #[cfg(test)]
                        || wait_at(gate, hooks.observed.as_ref()),
                    )),
                    StoreOp::CompactContext(read) => Input::CompactBuilt(context::read_compact(
                        open.then_some(&*side),
                        read,
                        #[cfg(test)]
                        || wait_at(gate, hooks.observed.as_ref()),
                    )),
                    StoreOp::ReadJob(read) => Input::JobBuilt(context::read_job(
                        open.then_some(&*side),
                        read,
                        #[cfg(test)]
                        || wait_at(gate, hooks.observed.as_ref()),
                    )),
                    StoreOp::ReadPage { key, page } => {
                        let request = key.request;
                        let reply = if open {
                            side.read_until(&key.control, |conn, control| {
                                control
                                    .with_handler(conn, |conn, scope| {
                                        scope
                                            .query(conn, || {
                                                #[cfg(test)]
                                                if let Some(gate) = gate {
                                                    hold(conn, &gate, hooks.observed.as_ref())?;
                                                }
                                                let read =
                                                    crate::store::context::Snapshot::begin(conn)?;
                                                let reply = read.page(request, page)?;
                                                read.end()?;
                                                Ok(reply)
                                            })
                                            .map_err(StoreError::from)
                                    })
                                    .map_err(StoreError::from)?
                            })
                        } else {
                            Err(StoreError::Closed)
                        };
                        if let Err(error) = &reply {
                            tracing::warn!(%error, "a page read failed");
                        }
                        Input::PageRead {
                            key,
                            reply: reply.unwrap_or(DataToShell::PageFailed { request }),
                        }
                    }
                    _ => unreachable!("StoreThread::send hands only reads to the reader"),
                };
                checkpoints.raise();
                if acks.send(reply).is_err() {
                    tracing::debug!("a page had no listener");
                }
            }
        })
}

/// Holds a build at its gate, its transaction open, until the gate releases it.
#[cfg(test)]
fn wait_at(gate: Option<super::Gate>, observed: Option<&UnboundedSender<super::super::Seen>>) {
    if let Some(gate) = gate {
        if let Some(observed) = observed {
            observed.send(super::super::Seen::ReadHeld).ok();
        }
        gate.release.recv().ok();
    }
}

/// Holds a read with a snapshot open on its connection until the gate releases it.
#[cfg(test)]
fn hold(
    conn: &rusqlite::Connection,
    gate: &super::Gate,
    observed: Option<&UnboundedSender<super::super::Seen>>,
) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN; SELECT count(*) FROM events;")?;
    if let Some(observed) = observed {
        observed.send(super::super::Seen::ReadHeld).ok();
    }
    gate.release.recv().ok();
    conn.execute_batch("COMMIT")?;
    Ok(())
}

/// Tells a test whether each checkpoint left nothing owed, then waits for its release.
#[cfg(test)]
pub(crate) type HelperGate = (mpsc::Sender<bool>, mpsc::Receiver<()>);

/// Checkpoints when raised, and again after a delay while frames are left, until stopped.
pub(super) fn spawn_helper(
    exit: &Exit,
    #[cfg(test)] gate: Option<HelperGate>,
) -> io::Result<JoinHandle<()>> {
    let (side, checkpoints) = (Arc::clone(&exit.side), Arc::clone(&exit.checkpoints));
    thread::Builder::new()
        .name("checkpoint".to_owned())
        .spawn(move || {
            let mut retry = None;
            while checkpoints.next(retry) {
                let left = match side.checkpoint() {
                    Ok(done) => !done,
                    // The op that opens the store again raises another.
                    Err(StoreError::Closed) => false,
                    Err(error) => {
                        tracing::warn!(%error, "a checkpoint failed; it's retried");
                        true
                    }
                };
                #[cfg(test)]
                if let Some((done, release)) = &gate {
                    done.send(!left).ok();
                    release.recv().ok();
                }
                retry = left.then(|| {
                    retry.map_or(FIRST_RETRY, |retry: Duration| (retry * 2).min(LAST_RETRY))
                });
            }
        })
}

mod context;
#[cfg(test)]
mod lifecycle_replay_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
