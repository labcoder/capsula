//! A bounded serve-side wait around a source owned by one disposable worker thread.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use super::elements::{
    self, ElementPlatform, ElementReader, ElementSource, ElementTree, MaskSink, TopWindow,
    Unavailable, WalkJob, Walked,
};
use super::{CaptureId, Failure};

pub(crate) const CONNECTION_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const TRANSACTION_TIMEOUT: Duration = Duration::from_millis(100);
const SLACK: Duration = CONNECTION_TIMEOUT.saturating_add(TRANSACTION_TIMEOUT);

#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "macOS adapter follows Windows")
)]
pub(crate) struct ThreadedReader<P> {
    platform: P,
    jobs: SyncSender<WalkJob>,
    replies: Receiver<(CaptureId, Result<ElementTree, Failure>)>,
    pending: Option<CaptureId>,
    retired: bool,
}

#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "macOS adapter follows Windows")
)]
impl<P: ElementPlatform> ThreadedReader<P> {
    pub(crate) fn new<S: ElementSource + 'static>(
        open: impl FnMut() -> Result<S, i32> + Send + 'static,
        platform: P,
    ) -> Result<Self, i32> {
        Self::with_clock(open, platform, Instant::now)
    }

    pub(crate) fn with_clock<S: ElementSource + 'static>(
        mut open: impl FnMut() -> Result<S, i32> + Send + 'static,
        platform: P,
        now: impl Fn() -> Instant + Send + 'static,
    ) -> Result<Self, i32> {
        let (jobs, requests) = mpsc::sync_channel::<WalkJob>(1);
        let (answers, replies) = mpsc::sync_channel(1);
        let mut worker_platform = platform.clone();
        let thread = std::thread::Builder::new()
            .spawn(move || {
                let mut source = None;
                while let Ok(job) = requests.recv() {
                    let result = match source.get_or_insert_with(&mut open) {
                        Ok(source) => elements::walk(
                            source,
                            &job,
                            &elements::LIMITS,
                            &now,
                            &mut worker_platform,
                        ),
                        Err(code) => {
                            if elements::finish_unavailable(&job.masks, Unavailable::Busy) {
                                Err(Failure::Fault(*code))
                            } else {
                                Err(Failure::Retire(*code))
                            }
                        }
                    };
                    if source.as_ref().is_some_and(Result::is_err) {
                        source = None;
                    }
                    if answers.try_send((job.capture, result)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| error.raw_os_error().unwrap_or(-1))?;
        drop(thread);
        Ok(Self {
            platform,
            jobs,
            replies,
            pending: None,
            retired: false,
        })
    }

    fn retire(&mut self, capture: CaptureId, masks: &MaskSink) -> Walked {
        self.retired = true;
        self.busy(capture, masks)
    }

    fn busy(&mut self, capture: CaptureId, masks: &MaskSink) -> Walked {
        let tree = ElementTree::unavailable(capture, Unavailable::Busy);
        let finished = elements::finish_unavailable(masks, Unavailable::Busy);
        self.retired |= !finished;
        Walked {
            tree,
            retire: self.retired,
        }
    }
}

impl<P: ElementPlatform> ElementReader for ThreadedReader<P> {
    fn set_shell_pid(&mut self, pid: u32) {
        self.platform.set_shell_pid(pid);
    }
    fn foreground(&mut self) -> Option<TopWindow> {
        self.platform.foreground()
    }

    fn walk(&mut self, job: WalkJob) -> Result<Walked, Failure> {
        let (capture, until, masks) = (job.capture, job.until, Arc::clone(&job.masks));
        if self.retired || masks.is_poisoned() {
            return Ok(self.retire(capture, &masks));
        }
        loop {
            match self.replies.try_recv() {
                Ok((old, _)) if self.pending == Some(old) => self.pending = None,
                Err(TryRecvError::Empty) => break,
                _ => return Ok(self.retire(capture, &masks)),
            }
        }
        if self.pending.is_some() {
            return Ok(self.retire(capture, &masks));
        }
        self.pending = Some(capture);
        if let Err(error) = self.jobs.try_send(job) {
            self.retired = matches!(error, mpsc::TrySendError::Disconnected(_));
            return Ok(self.busy(capture, &masks));
        }
        let timeout = (until + SLACK).saturating_duration_since(Instant::now());
        match self.replies.recv_timeout(timeout) {
            Ok((received, result)) if received == capture && !masks.is_poisoned() => {
                self.pending = None;
                if !matches!(self.replies.try_recv(), Err(TryRecvError::Empty)) {
                    return Ok(self.retire(capture, &masks));
                }
                self.retired |= matches!(&result, Err(Failure::Retire(_)));
                result.map(|tree| Walked {
                    tree,
                    retire: false,
                })
            }
            Err(RecvTimeoutError::Timeout) if !masks.is_poisoned() => {
                Ok(self.busy(capture, &masks))
            }
            _ => Ok(self.retire(capture, &masks)),
        }
    }
}

#[cfg(test)]
mod tests;
