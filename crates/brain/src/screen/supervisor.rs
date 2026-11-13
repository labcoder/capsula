use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::geometry::DisplayRef;
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::elements::ElementTree;
use super::wire::{
    CaptureId, ElementTarget, Exclusion, FrameMeta, MAX_REPLY_BYTES, Refusal, ReplyBody,
    RequestBody, ScreenError, TicketWire,
};
use crate::platform::Job;
use crate::supervise::{self, Generation, Handle, Kind};

/// Absolute request budget, including queueing (guess).
pub const REQUEST_DEADLINE: Duration = Duration::from_millis(1500);

/// Why a capture or its element walk was refused or lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The worker refused the ticket or the display.
    #[error("capture refused: {0:?}")]
    Refused(Refusal),
    /// The worker failed or was lost.
    #[error(transparent)]
    Screen(#[from] ScreenError),
}

/// The screen worker's kind for the shared supervisor.
pub(crate) struct Screen;

impl Kind for Screen {
    const ARG: &'static str = crate::SCREEN_WORKER;
    const NAME: &'static str = "screen";
    /// Requests are a tag, a fixture or a capture's ticket and display at most.
    const MAX_REQUEST: usize = 64 * 1024;
    const MAX_REPLY: usize = MAX_REPLY_BYTES;
    type Request = RequestBody;
    type Reply = ReplyBody;
    type Assembly = crate::supervise::Zeroed;
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
    /// Returns `WorkerLost` on a deadline, transport failure, overload or shutdown,
    /// `ExitUnconfirmed` for good once a lost worker wasn't seen to exit, and `Fault`
    /// when the worker refused the operation.
    pub async fn request(&self, body: RequestBody) -> Result<ReplyBody, ScreenError> {
        match self
            .inner
            .request(body, Instant::now() + self.deadline)
            .await
        {
            Ok((ReplyBody::Fault(code), _)) => Err(ScreenError::Fault(code)),
            Ok((reply, _)) => Ok(reply),
            Err(lost) => Err(lost.into()),
        }
    }

    /// Takes one frame under `ticket`; the pixels stay in the worker. The worker's validity runs
    /// from its receipt, so the caller (B2a) still discards a reply past the ticket's own deadline.
    ///
    /// # Errors
    /// The worker's refusal, a platform fault, or `WorkerLost` for any reply that isn't this
    /// capture's.
    pub async fn capture(
        &self,
        ticket: TicketWire,
        display: DisplayRef,
        exclude: Exclusion,
    ) -> Result<FrameMeta, CaptureError> {
        let capture = ticket.capture();
        let body = RequestBody::Capture {
            ticket: ticket.for_shell(crate::platform::connected_shell_pid()),
            display,
            exclude,
        };
        match self.request(body).await? {
            ReplyBody::Captured(meta) if meta.capture == capture && meta.display == display.0 => {
                Ok(meta)
            }
            ReplyBody::Refused(refusal) => Err(CaptureError::Refused(refusal)),
            _ => Err(ScreenError::WorkerLost.into()),
        }
    }

    /// Reads the held capture's control view once.
    ///
    /// # Errors
    /// A refusal, a numeric fault, or `WorkerLost` for a reply from another capture.
    // shortcut: A6 or B2a will submit Elements in production.
    pub async fn elements(
        &self,
        capture: CaptureId,
        target: ElementTarget,
    ) -> Result<ElementTree, CaptureError> {
        match self
            .request(RequestBody::Elements { capture, target })
            .await?
        {
            ReplyBody::Elements(tree) if tree.capture == capture => Ok(tree),
            ReplyBody::Refused(refusal) => Err(CaptureError::Refused(refusal)),
            _ => Err(ScreenError::WorkerLost.into()),
        }
    }

    /// Subscribes before issuing work, so a lost watch generation cannot be missed.
    pub fn lost(&self) -> broadcast::Receiver<Generation> {
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

#[cfg(test)]
mod tests;
