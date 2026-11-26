use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, Id, Runtime};
use tokio::sync::{Notify, Semaphore};
use tokio::task::AbortHandle;

mod trust;

#[cfg(test)]
mod tests;

const MAX_CONNECTS: usize = 4;
const CONNECT_CAP: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
enum Refusal {
    #[error("connection cancelled")]
    Cancelled,
    #[error("connection deadline elapsed")]
    Deadline,
    #[error("connection executor busy")]
    Busy,
    #[error("connection executor unavailable")]
    Unavailable,
}

#[derive(Default)]
struct Stop {
    closed: AtomicBool,
    changed: Notify,
}

impl Stop {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    fn check(&self) -> Result<(), Refusal> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Refusal::Cancelled);
        }
        Ok(())
    }

    async fn until_closed(&self) {
        let changed = self.changed.notified();
        if self.check().is_ok() {
            changed.await;
        }
    }
}

struct Attempt {
    request: Arc<Stop>,
    abandoned: Arc<Stop>,
    by: Instant,
    owner: Id,
    checks: Arc<Semaphore>,
}

impl Attempt {
    fn check(&self) -> Result<(), Refusal> {
        self.request.check()?;
        self.abandoned.check()?;
        if Instant::now() >= self.by {
            return Err(Refusal::Deadline);
        }
        Ok(())
    }
}

tokio::task_local! {
    static CONNECTING: Arc<Attempt>;
}

fn evaluate<T>(verify: impl FnOnce() -> T) -> Result<T, Refusal> {
    let attempt = CONNECTING
        .try_with(Arc::clone)
        .map_err(|_| Refusal::Unavailable)?;
    if !Handle::try_current().is_ok_and(|worker| worker.id() == attempt.owner) {
        return Err(Refusal::Unavailable);
    }
    let _slot = attempt.checks.try_acquire().map_err(|_| Refusal::Busy)?;
    attempt.check()?;
    let verdict = tokio::task::block_in_place(verify);
    attempt.check()?;
    Ok(verdict)
}

struct Executor {
    runtime: Option<Runtime>,
    connects: Arc<Semaphore>,
    checks: Arc<Semaphore>,
}

impl Executor {
    fn new() -> Result<Self, Refusal> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|_| Refusal::Unavailable)?;
        Ok(Self {
            runtime: Some(runtime),
            connects: Arc::new(Semaphore::new(MAX_CONNECTS)),
            checks: Arc::new(Semaphore::new(1)),
        })
    }

    async fn connect<T: Send + 'static>(
        &self,
        request: Arc<Stop>,
        by: Instant,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, Refusal> {
        let runtime = self.runtime.as_ref().ok_or(Refusal::Unavailable)?;
        let by = by.min(Instant::now() + CONNECT_CAP);
        let attempt = Arc::new(Attempt {
            request,
            abandoned: Arc::new(Stop::default()),
            by,
            owner: runtime.handle().id(),
            checks: Arc::clone(&self.checks),
        });
        attempt.check()?;
        if self.checks.available_permits() == 0 {
            return Err(Refusal::Busy);
        }
        let slot = Arc::clone(&self.connects)
            .try_acquire_owned()
            .map_err(|_| Refusal::Busy)?;
        let worker = Arc::clone(&attempt);
        let task = runtime.spawn(CONNECTING.scope(Arc::clone(&worker), async move {
            let _slot = slot;
            worker.check()?;
            let result = work.await;
            worker.check()?;
            Ok(result)
        }));
        let _abandon = Abandon {
            stop: Arc::clone(&attempt.abandoned),
            abort: task.abort_handle(),
        };
        let result = tokio::select! {
            biased;
            () = attempt.request.until_closed() => Err(Refusal::Cancelled),
            () = tokio::time::sleep_until(by.into()) => Err(Refusal::Deadline),
            answer = task => answer.map_err(|_| Refusal::Unavailable)?,
        }?;
        attempt.check()?;
        Ok(result)
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

struct Abandon {
    stop: Arc<Stop>,
    abort: AbortHandle,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.stop.close();
        self.abort.abort();
    }
}
