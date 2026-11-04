use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::Instant;

use super::wire::{MAX_REPLY_BYTES, ReplyBody, RequestBody, ScreenError};
use crate::platform::Job;
use crate::supervise::{self, Handle, Kind, WorkerLost};

/// Absolute request budget, including queueing (guess).
pub const REQUEST_DEADLINE: Duration = Duration::from_millis(1500);

/// The screen worker's kind for the shared supervisor.
pub(crate) struct Screen;

impl Kind for Screen {
    const ARG: &'static str = crate::SCREEN_WORKER;
    const NAME: &'static str = "screen";
    /// Requests are a tag and a fixture at most.
    const MAX_REQUEST: usize = 64 * 1024;
    const MAX_REPLY: usize = MAX_REPLY_BYTES;
    type Request = RequestBody;
    type Reply = ReplyBody;
}

/// The isolated worker's lifecycle owner.
#[derive(Debug)]
pub struct ScreenSupervisor;

/// Requests and loss notifications; dropping the last handle stops the worker.
#[derive(Clone, Debug)]
pub struct ScreenHandle {
    inner: Handle<Screen>,
    deadline: Duration,
}

impl ScreenHandle {
    /// Submits one operation. Backoff and a full queue fail fast.
    ///
    /// # Errors
    /// Returns `WorkerLost` on a deadline, transport failure, overload or shutdown, and `Fault`
    /// when the worker refused the operation.
    pub async fn request(&self, body: RequestBody) -> Result<ReplyBody, ScreenError> {
        match self
            .inner
            .request(body, Instant::now() + self.deadline)
            .await
        {
            Ok(ReplyBody::Fault(code)) => Err(ScreenError::Fault(code)),
            Ok(reply) => Ok(reply),
            Err(WorkerLost) => Err(ScreenError::WorkerLost),
        }
    }

    /// Subscribes before issuing work, so a lost watch generation cannot be missed.
    pub fn lost(&self) -> broadcast::Receiver<WorkerLost> {
        self.inner.lost()
    }
}

impl ScreenSupervisor {
    /// Starts the worker directly in the brain job. macOS uses `posix_spawn` file actions
    /// and a process-group kill behind the same platform boundary.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn start(exe: PathBuf, jobs: Arc<Job>) -> io::Result<ScreenHandle> {
        Self::with_deadline(exe, jobs, REQUEST_DEADLINE)
    }

    /// Starts with an injected budget so fixtures do not race a product deadline.
    ///
    /// # Errors
    /// Returns a process or thread creation failure.
    pub fn with_deadline(
        exe: PathBuf,
        jobs: Arc<Job>,
        deadline: Duration,
    ) -> io::Result<ScreenHandle> {
        Ok(ScreenHandle {
            inner: supervise::start::<Screen>(exe, jobs)?,
            deadline,
        })
    }
}
