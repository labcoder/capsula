//! A bounded serve-side wait around a source owned by one disposable worker thread.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use super::elements::{
    self, ElementPlatform, ElementReader, ElementSource, ElementTree, MaskSink, Unavailable,
    WalkJob, Walked,
};
use super::{CaptureId, Failure};

pub(crate) const CONNECTION_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const TRANSACTION_TIMEOUT: Duration = Duration::from_millis(100);

/// The walk thread's one completion: it finishes at most one job at a time, and `begin` refuses a
/// job while the last one's completion hasn't been taken, so this one-slot channel never fills.
type Completion = (CaptureId, Result<ElementTree, Failure>);

#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "macOS adapter follows Windows")
)]
pub(crate) struct ThreadedReader<P> {
    platform: P,
    jobs: SyncSender<WalkJob>,
    replies: Receiver<Completion>,
    /// The job sent whose completion hasn't been taken.
    pending: Option<(CaptureId, MaskSink)>,
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
        wake: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self, i32> {
        Self::with_clock(open, platform, Instant::now, wake)
    }

    /// Calls `wake` after each completion lands in its slot. `wake` must not block: a full
    /// listener drops the wake and the completion waits for `poll`. `false` means the listener
    /// is gone, so the thread ends.
    pub(crate) fn with_clock<S: ElementSource + 'static>(
        mut open: impl FnMut() -> Result<S, i32> + Send + 'static,
        platform: P,
        now: impl Fn() -> Instant + Send + 'static,
        wake: impl Fn() -> bool + Send + 'static,
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
                    if answers.try_send((job.capture, result)).is_err() || !wake() {
                        drop(requests);
                        drop(answers);
                        return;
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

    /// Ends the pending walk with its completion; a missing, foreign or extra completion, or
    /// poisoned masks, retire the reader.
    fn settle(
        &mut self,
        capture: CaptureId,
        masks: &MaskSink,
        reply: Option<Completion>,
    ) -> Result<Walked, Failure> {
        match reply {
            Some((received, result)) if received == capture && !masks.is_poisoned() => {
                self.pending = None;
                if !matches!(self.replies.try_recv(), Err(TryRecvError::Empty)) {
                    return Ok(self.retire(capture, masks));
                }
                self.retired |= matches!(&result, Err(Failure::Retire(_)));
                result.map(|tree| Walked {
                    tree,
                    retire: self.retired,
                })
            }
            _ => Ok(self.retire(capture, masks)),
        }
    }
}

impl<P: ElementPlatform> ElementReader for ThreadedReader<P> {
    fn set_shell_pid(&mut self, pid: u32) {
        self.platform.set_shell_pid(pid);
    }
    fn begin(&mut self, job: WalkJob) -> Result<(), Walked> {
        let (capture, masks) = (job.capture, Arc::clone(&job.masks));
        if self.retired || masks.is_poisoned() || self.pending.is_some() {
            return Err(self.retire(capture, &masks));
        }
        self.pending = Some((capture, Arc::clone(&masks)));
        if let Err(error) = self.jobs.try_send(job) {
            self.retired = matches!(error, mpsc::TrySendError::Disconnected(_));
            return Err(self.busy(capture, &masks));
        }
        Ok(())
    }

    fn poll(&mut self) -> Option<Result<Walked, Failure>> {
        let (capture, masks) = self.pending.clone()?;
        match self.replies.try_recv() {
            Err(TryRecvError::Empty) if !masks.is_poisoned() => None,
            reply => Some(self.settle(capture, &masks, reply.ok())),
        }
    }
}

/// A walk begun and waited out between polls, as the actor's loop does between wakes. Fixtures only.
#[cfg(test)]
pub(crate) trait Walk {
    fn walk(&mut self, job: WalkJob) -> Result<Walked, Failure>;
}

#[cfg(test)]
impl<P: ElementPlatform> Walk for ThreadedReader<P> {
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a walk that never answers fails the test"
    )]
    fn walk(&mut self, job: WalkJob) -> Result<Walked, Failure> {
        if let Err(walked) = self.begin(job) {
            return Ok(walked);
        }
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(answer) = self.poll() {
                return answer;
            }
            assert!(Instant::now() < until, "the walk never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests;
