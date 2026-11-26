use std::future::Future;
use std::io;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, Runtime};
use tokio::sync::{Semaphore, oneshot};

use crate::platform::tls_control::{ConnectCut, Control};

mod connect;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod trust;
#[cfg(test)]
use {crate::platform::tls_control::ConnectCut as Stop, tokio::runtime::Id, tokio::sync::Notify};

pub use connect::NativeStream;
pub(super) use connect::{direct, http};

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

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        let kind = match refusal {
            Refusal::Cancelled => io::ErrorKind::Interrupted,
            Refusal::Deadline => io::ErrorKind::TimedOut,
            Refusal::Busy => io::ErrorKind::WouldBlock,
            Refusal::Unavailable => io::ErrorKind::NotConnected,
        };
        Self::from(kind)
    }
}

struct Attempt {
    request: Control,
    abandoned: Arc<ConnectCut>,
    by: Instant,
    #[cfg(test)]
    owner: Id,
    #[cfg(test)]
    checks: Arc<Semaphore>,
}

impl Attempt {
    fn check(&self) -> Result<(), Refusal> {
        if self.request.cancelled() || self.abandoned.closed() {
            return Err(Refusal::Cancelled);
        }
        if Instant::now() >= self.by {
            return Err(Refusal::Deadline);
        }
        Ok(())
    }

    fn with<T>(&self, poll: impl FnOnce() -> T) -> io::Result<T> {
        self.request.with(|| {
            self.abandoned.with(|| {
                if Instant::now() >= self.by {
                    return Err(Refusal::Deadline.into());
                }
                Ok(poll())
            })?
        })
    }
}

tokio::task_local! {
    static CONNECTING: Arc<Attempt>;
}

#[cfg(test)]
fn evaluate<T>(verify: impl FnOnce() -> T) -> Result<T, Refusal> {
    let attempt = CONNECTING
        .try_with(Arc::clone)
        .map_err(|_| Refusal::Unavailable)?;
    if !Handle::try_current().is_ok_and(|worker| worker.id() == attempt.owner) {
        return Err(Refusal::Unavailable);
    }
    let _slot = attempt.checks.try_acquire().map_err(|_| Refusal::Busy)?;
    attempt.with(|| ()).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut => Refusal::Deadline,
        _ => Refusal::Cancelled,
    })?;
    let verdict = verify();
    attempt.check()?;
    Ok(verdict)
}

struct Executor {
    reactor: Handle,
    workers: Vec<Worker>,
    #[cfg(test)]
    checks: Arc<Semaphore>,
    reactor_stop: Arc<oneshot::Sender<()>>,
}

type Job = Box<dyn FnOnce() + Send>;

struct Worker {
    job: mpsc::SyncSender<Job>,
    idle: Arc<Semaphore>,
}

async fn establish<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> io::Result<T> {
    static EXECUTOR: OnceLock<Result<Executor, Refusal>> = OnceLock::new();
    let request = Control::current()?;
    EXECUTOR
        .get_or_init(Executor::new)
        .as_ref()
        .map_err(|refusal| *refusal)?
        .establish(request, work)
        .await
        .map_err(Into::into)
}

impl Executor {
    fn new() -> Result<Self, Refusal> {
        Self::with_start(|job| thread::Builder::new().spawn(job).map(drop))
    }

    fn with_start(mut start: impl FnMut(Job) -> io::Result<()>) -> Result<Self, Refusal> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Refusal::Unavailable)?;
        let reactor = runtime.handle().clone();
        let driver = Driver(Some(runtime));
        let (shutdown, until_shutdown) = oneshot::channel();
        start(Box::new(move || driver.run(until_shutdown))).map_err(|_| Refusal::Unavailable)?;

        let mut workers = Vec::with_capacity(MAX_CONNECTS);
        for _ in 0..MAX_CONNECTS {
            let (job, jobs) = mpsc::sync_channel::<Job>(1);
            start(Box::new(move || {
                for job in jobs {
                    job();
                }
            }))
            .map_err(|_| Refusal::Unavailable)?;
            workers.push(Worker {
                job,
                idle: Arc::new(Semaphore::new(1)),
            });
        }
        Ok(Self {
            reactor,
            workers,
            #[cfg(test)]
            checks: Arc::new(Semaphore::new(1)),
            reactor_stop: Arc::new(shutdown),
        })
    }

    #[cfg(test)]
    async fn connect<T: Send + 'static>(
        &self,
        request: Arc<Stop>,
        by: Instant,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, Refusal> {
        self.establish(Control::fixture(by, request), work).await
    }

    async fn establish<T: Send + 'static>(
        &self,
        request: Control,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, Refusal> {
        let by = request.deadline(CONNECT_CAP);
        let attempt = Arc::new(Attempt {
            request,
            abandoned: Arc::new(ConnectCut::default()),
            by,
            #[cfg(test)]
            owner: self.reactor.id(),
            #[cfg(test)]
            checks: Arc::clone(&self.checks),
        });
        attempt.check()?;
        #[cfg(test)]
        if self.checks.available_permits() == 0 {
            return Err(Refusal::Busy);
        }
        let (sender, slot) = self
            .workers
            .iter()
            .find_map(|worker| {
                Arc::clone(&worker.idle)
                    .try_acquire_owned()
                    .ok()
                    .map(|slot| (&worker.job, slot))
            })
            .ok_or(Refusal::Busy)?;
        let (send, answer) = oneshot::channel();
        let reactor = self.reactor.clone();
        let reactor_stop = Arc::clone(&self.reactor_stop);
        let worker = Arc::clone(&attempt);
        sender
            .try_send(Box::new(move || {
                let _slot = slot;
                let _reactor_stop = reactor_stop;
                let result = reactor.block_on(CONNECTING.scope(Arc::clone(&worker), async move {
                    worker.check()?;
                    let result = tokio::select! {
                        biased;
                        () = worker.request.until_cancelled() => return Err(Refusal::Cancelled),
                        () = worker.abandoned.until_closed() => return Err(Refusal::Cancelled),
                        () = tokio::time::sleep_until(by.into()) => return Err(Refusal::Deadline),
                        outcome = work => outcome,
                    };
                    worker.check()?;
                    Ok(result)
                }));
                let _unheard = send.send(result);
            }))
            .map_err(|_| Refusal::Unavailable)?;
        let _abandon = Abandon(Arc::clone(&attempt.abandoned));
        let result = tokio::select! {
            biased;
            () = attempt.request.until_cancelled() => Err(Refusal::Cancelled),
            () = tokio::time::sleep_until(by.into()) => Err(Refusal::Deadline),
            result = answer => result.map_err(|_| Refusal::Unavailable)?,
        }?;
        attempt.check()?;
        Ok(result)
    }
}

struct Driver(Option<Runtime>);

impl Driver {
    fn run(self, shutdown: oneshot::Receiver<()>) {
        if let Some(runtime) = &self.0 {
            let _closed = runtime.block_on(shutdown);
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

struct Abandon(Arc<ConnectCut>);

impl Drop for Abandon {
    fn drop(&mut self) {
        self.0.close();
    }
}
