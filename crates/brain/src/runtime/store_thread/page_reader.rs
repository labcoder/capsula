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

use kumochi_protocol::message::{DataToShell, PageOf};
use tokio::sync::mpsc::UnboundedSender;

use crate::coordinator::Input;
use crate::store::{Pages, Side, StoreError};

/// The helper's wait after a checkpoint that left frames behind, doubling up to `LAST_RETRY`
/// [guess].
const FIRST_RETRY: Duration = Duration::from_millis(50);
const LAST_RETRY: Duration = Duration::from_secs(1);

/// A page read, stamped with how many other ops were accepted before it.
#[derive(Debug)]
pub(super) struct Read {
    pub(super) stamp: u64,
    pub(super) epoch: u64,
    pub(super) request: u32,
    pub(super) page: PageOf,
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
}

impl Barrier {
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
    pub(super) fn wait(&self, stamp: u64) -> bool {
        let progress = self
            .moved
            .wait_while(lock(&self.progress), |progress| {
                progress.completed < stamp && !progress.terminal
            })
            .unwrap_or_else(PoisonError::into_inner);
        !progress.terminal
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
            for Read {
                stamp,
                epoch,
                request,
                page,
            } in reads
            {
                #[cfg(test)]
                let gate = {
                    let op = crate::coordinator::StoreOp::ReadPage {
                        epoch,
                        request,
                        page: page.clone(),
                    };
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
                let reply = if barrier.wait(stamp) {
                    side.read(|conn| {
                        #[cfg(test)]
                        if let Some(gate) = gate {
                            hold(conn, &gate, hooks.observed.as_ref())?;
                        }
                        Pages(conn).read_page(request, page)
                    })
                } else {
                    Err(StoreError::Closed)
                };
                checkpoints.raise();
                if let Err(error) = &reply {
                    tracing::warn!(%error, "a page read failed");
                }
                let reply = Input::PageRead {
                    epoch,
                    request,
                    reply: reply.unwrap_or(DataToShell::PageFailed { request }),
                };
                if acks.send(reply).is_err() {
                    tracing::debug!("a page had no listener");
                }
            }
        })
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

/// Checkpoints when raised, and again after a delay while frames are left, until stopped.
pub(super) fn spawn_helper(exit: &Exit) -> io::Result<JoinHandle<()>> {
    let (side, checkpoints) = (Arc::clone(&exit.side), Arc::clone(&exit.checkpoints));
    thread::Builder::new()
        .name("checkpoint".to_owned())
        .spawn(move || {
            let mut retry = None;
            while checkpoints.next(retry) {
                let left = match side.checkpoint() {
                    Ok(done) => !done,
                    Err(error) => {
                        tracing::warn!(%error, "a checkpoint failed; it's retried");
                        true
                    }
                };
                retry = left.then(|| {
                    retry.map_or(FIRST_RETRY, |retry: Duration| (retry * 2).min(LAST_RETRY))
                });
            }
        })
}

#[cfg(test)]
mod tests;
