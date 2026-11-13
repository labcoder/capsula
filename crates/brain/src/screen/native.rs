//! One capture producer: its non-Send capturer never leaves its dedicated thread.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::Instant;

use kumochi_protocol::geometry::PhysicalPoint;

use super::actor::Key;
use super::capture::{Acquired, Capturer, Failure};
use super::elements::{TopWindow, Wake};
use super::frame::RawFrame;

/// One hold's capture: the window under `point`, then one frame of it alone.
pub(super) struct Job {
    pub(super) key: Key,
    pub(super) frame: RawFrame,
    pub(super) point: PhysicalPoint,
    /// The connected shell's process, which the walk counts as Kumochi's.
    pub(super) shell: u32,
    pub(super) until: Instant,
}

/// The frame comes back on every outcome; a success names the window it shows.
pub(super) type Completion = (Key, RawFrame, Result<(Acquired, TopWindow), Failure>);

pub(super) struct CaptureThread {
    jobs: SyncSender<Job>,
    replies: Receiver<Completion>,
    pending: bool,
}

impl CaptureThread {
    pub(super) fn open(
        mut open: impl FnMut() -> Result<Box<dyn Capturer>, i32> + Send + 'static,
        wake: Wake,
    ) -> Result<Self, i32> {
        let (jobs, requests) = mpsc::sync_channel::<Job>(1);
        let (answers, replies) = mpsc::sync_channel(1);
        drop(
            std::thread::Builder::new()
                .name("screen-capture".into())
                .spawn(move || {
                    let mut capturer = None;
                    while let Ok(mut job) = requests.recv() {
                        let result = (|| {
                            let capturer = match &mut capturer {
                                Some(capturer) => capturer,
                                None => capturer.insert(open().map_err(Failure::Fault)?),
                            };
                            let target = capturer.pointer_window(job.point, job.shell)?;
                            capturer
                                .capture_window(&target, job.until, &mut job.frame)
                                .map(|acquired| (acquired, target.top))
                        })();
                        if answers.try_send((job.key, job.frame, result)).is_err() || !wake() {
                            drop(requests);
                            drop(answers);
                            return;
                        }
                    }
                })
                .map_err(|error| error.raw_os_error().unwrap_or(-1))?,
        );
        Ok(Self {
            jobs,
            replies,
            pending: false,
        })
    }

    pub(super) fn begin(&mut self, job: Job) -> Option<Job> {
        if self.pending {
            return Some(job);
        }
        match self.jobs.try_send(job) {
            Ok(()) => {
                self.pending = true;
                None
            }
            Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) => Some(job),
        }
    }

    pub(super) fn poll(&mut self) -> Result<Option<Completion>, ()> {
        if !self.pending {
            return Ok(None);
        }
        match self.replies.try_recv() {
            Ok(reply) => {
                self.pending = false;
                Ok(Some(reply))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }
}

#[cfg(test)]
mod tests;
