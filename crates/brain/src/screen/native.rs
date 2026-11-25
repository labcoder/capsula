//! One capture producer: its non-Send capturer never leaves its dedicated thread.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::Instant;

use kumochi_protocol::geometry::PhysicalPoint;

use super::actor::Key;
use super::capture::{Acquired, Capturer, Deadline, Failure, Shown};
use super::elements::Wake;
use super::frame::RawFrame;
use super::wire::Refusal;

/// What a job captures at its point.
pub(super) enum Aim {
    /// The window under it, then one frame of it alone. `shell` is the connected shell's process,
    /// which the walk counts as Kumochi's.
    Window { shell: u32 },
    /// One frame of the monitor nearest it, drawn after `excluded`, the shell's exclusion stamp.
    Display { excluded: u64 },
}

/// One hold's capture.
pub(super) struct Job {
    pub(super) key: Key,
    pub(super) frame: RawFrame,
    pub(super) point: PhysicalPoint,
    pub(super) aim: Aim,
    pub(super) deadline: Deadline,
}

/// The frame comes back on every outcome; a success names what it shows.
pub(super) type Completion = (Key, RawFrame, Result<(Acquired, Shown), Failure>);

pub(super) struct CaptureThread {
    jobs: SyncSender<Job>,
    replies: Receiver<Completion>,
    pending: bool,
}

impl CaptureThread {
    pub(super) fn open(
        mut open: impl FnMut() -> Result<Box<dyn Capturer>, Failure> + Send + 'static,
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
                                None => capturer.insert(open()?),
                            };
                            let until = || {
                                job.deadline
                                    .tighten(Instant::now())
                                    .map(|deadline| deadline.until)
                                    .ok_or(Refusal::Expired)
                            };
                            match job.aim {
                                Aim::Window { shell } => {
                                    let target = capturer.pointer_window(job.point, shell)?;
                                    capturer
                                        .capture_window(&target, until()?, &mut job.frame)
                                        .map(|acquired| (acquired, Shown::Window(target)))
                                }
                                Aim::Display { excluded } => capturer
                                    .capture_display(job.point, excluded, until()?, &mut job.frame)
                                    .map(|(acquired, bounds)| {
                                        let dpi = acquired.dpi;
                                        (acquired, Shown::Display { bounds, dpi })
                                    }),
                            }
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
