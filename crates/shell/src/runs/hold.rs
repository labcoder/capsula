use kumochi_protocol::message::{ControlToBrain, GestureId, RunId, RunOrigin, ThreadId};

use super::{Applied, Refused, Runs, Target};

#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    pub(super) gesture: GestureId,
    pub(super) revision: u64,
    pub(super) thread: Option<ThreadId>,
    pub(super) run: Option<RunId>,
}

impl Runs {
    pub(crate) fn begin_hold(&mut self, gesture: GestureId) -> Result<ControlToBrain, Refused> {
        if gesture.0 <= self.hold_high {
            return Err(Refused::Busy);
        }
        self.hold_high = gesture.0;
        if !self.connected || self.main.is_none() {
            return Err(Refused::Offline);
        }
        if self.hold.is_some()
            || self.hold_cancel.is_some()
            || (self.target == Target::New
                && self.submitted.iter().any(|s| s.revision == self.revision))
        {
            return Err(Refused::Busy);
        }
        self.forget_texts(|_| true);
        self.hold = Some(Hold {
            gesture,
            revision: self.revision,
            thread: self.thread(),
            run: None,
        });
        Ok(ControlToBrain::VoiceHoldRecognized {
            gesture,
            thread: self.thread(),
            selection: self.revision,
        })
    }

    pub(crate) fn cancel_hold(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.hold_cancel = Some(hold.gesture);
            self.fence = self.fence.max(hold.run.unwrap_or_default());
        }
    }

    pub(crate) fn take_hold_cancel(&mut self) -> Option<ControlToBrain> {
        self.hold_cancel
            .take()
            .map(|gesture| ControlToBrain::GestureCancel { gesture })
    }

    pub(super) fn apply_hold_state(
        &mut self,
        run: RunId,
        thread: ThreadId,
        origin: Option<RunOrigin>,
        applied: &mut Applied,
    ) {
        let Some(RunOrigin::Hold { gesture }) = origin else {
            return;
        };
        if self.hold_high == 0 {
            return;
        }
        if self.live.is_some_and(|live| live.run == run) {
            return;
        }
        let matches = self.hold.is_some_and(|hold| {
            hold.gesture == gesture
                && hold.run.is_none()
                && run > self.answered
                && run > self.fence
                && hold.revision == self.revision
                && hold.thread == self.thread()
                && hold.thread.is_none_or(|id| id == thread)
        }) && !self.holds(thread);
        if matches {
            if self.target == Target::New {
                self.target = if Some(thread) == self.main {
                    Target::Kumo
                } else {
                    Target::Task(thread)
                };
            }
            if let Some(hold) = &mut self.hold {
                hold.run = Some(run);
            }
        } else if run > self.fence {
            self.fence = run;
            applied.stop = Some(ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            });
            if self.hold.is_some_and(|hold| hold.gesture == gesture) {
                self.cancel_hold();
            }
        }
    }
}
