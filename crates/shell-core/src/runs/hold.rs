use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{
    ControlToBrain, GestureId, HeldScreen, LeaseGeneration, ModelName, RunId, RunOrigin,
    ScreenClass, ScreenHolds, ScreenOff, ThreadId,
};

use super::{Applied, Refused, Runs, Target};

/// What makes a hold a screen hold: the pointer sampled at recognition, the exclusion lease
/// protecting the capture, and when the hold was recognized. It has no `Debug`, so the pointer
/// is never printed.
#[derive(Clone, Copy)]
pub struct ScreenHold {
    pub pointer: PhysicalPoint,
    pub lease: LeaseGeneration,
    pub t_qpc: u64,
}

/// What a screen hold's own line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenLine {
    /// The prediction, from recognition until the brain says what the turn carried.
    WillSee,
    NotIncluded(ScreenClass),
}

/// The standing notice: what a hold on the current target does with the window under the
/// pointer, and the only model it may name. A hold is a screen hold only while this is `On`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenNotice {
    /// Nothing shows: this system can't capture a window.
    Hidden,
    /// A hold shows this model the window.
    On(ModelName),
    /// The default model, named here, can't be sent images.
    ModelCantSee(ModelName),
    NotReady,
    TurnedOff,
    /// The target thread runs on its own model, which no `ScreenHolds` state speaks for.
    OwnModel,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    pub(super) gesture: GestureId,
    pub(super) revision: u64,
    pub(super) thread: Option<ThreadId>,
    pub(super) run: Option<RunId>,
    /// Set only for a screen hold. Never the pointer: `Runs` prints this.
    pub(super) screen: Option<ScreenLine>,
}

impl Runs {
    /// The brain said whether a hold also shows the model a window.
    pub fn set_screen_holds(&mut self, state: ScreenHolds) {
        self.screen_holds = Some(state);
    }

    /// This connection's last `ScreenHolds`: none before it arrives, and none once it ends.
    pub fn screen_holds(&self) -> Option<&ScreenHolds> {
        self.screen_holds.as_ref()
    }

    /// The notice for a hold on the current target. `own_model` says the target thread runs on
    /// a model other than the default; `default` is the newest default model's name in the
    /// shell's own catalog. A stored `On` naming any other model is out of date, because the
    /// default just changed, and reads as not ready.
    pub fn screen_notice(&self, own_model: bool, default: Option<&ModelName>) -> ScreenNotice {
        match &self.screen_holds {
            Some(ScreenHolds::Off(ScreenOff::Unsupported)) => ScreenNotice::Hidden,
            Some(ScreenHolds::Off(ScreenOff::TurnedOff)) => ScreenNotice::TurnedOff,
            _ if own_model => ScreenNotice::OwnModel,
            Some(ScreenHolds::On { model }) if Some(model) == default => {
                ScreenNotice::On(model.clone())
            }
            Some(ScreenHolds::Off(ScreenOff::ModelCantSee)) => default
                .map_or(ScreenNotice::NotReady, |model| {
                    ScreenNotice::ModelCantSee(model.clone())
                }),
            Some(ScreenHolds::On { .. } | ScreenHolds::Off(ScreenOff::NotReady)) | None => {
                ScreenNotice::NotReady
            }
        }
    }

    /// The current hold's line about the screen; it goes when the hold does.
    pub fn hold_screen_line(&self) -> Option<ScreenLine> {
        self.hold.and_then(|hold| hold.screen)
    }

    /// The brain said what `gesture`'s screen hold carried into its turn. False, and nothing
    /// changes, unless that is the current hold and it still shows the prediction.
    pub fn hold_screen(&mut self, gesture: GestureId, outcome: HeldScreen) -> bool {
        let Some(hold) = self
            .hold
            .as_mut()
            .filter(|hold| hold.gesture == gesture && hold.screen == Some(ScreenLine::WillSee))
        else {
            return false;
        };
        hold.screen = match outcome {
            HeldScreen::NotIncluded(class) => Some(ScreenLine::NotIncluded(class)),
            // History carries these.
            HeldScreen::Attached | HeldScreen::Withdrawn => None,
        };
        true
    }

    /// Starts a hold: a screen hold when `screen` is given, else a voice-only one.
    pub fn begin_hold(
        &mut self,
        gesture: GestureId,
        screen: Option<ScreenHold>,
    ) -> Result<ControlToBrain, Refused> {
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
        let thread = self.thread();
        let selection = self.revision;
        self.hold = Some(Hold {
            gesture,
            revision: selection,
            thread,
            run: None,
            screen: screen.map(|_| ScreenLine::WillSee),
        });
        Ok(match screen {
            Some(ScreenHold {
                pointer,
                lease,
                t_qpc,
            }) => ControlToBrain::HoldRecognized {
                gesture,
                thread,
                selection,
                pointer,
                lease,
                t_qpc,
            },
            None => ControlToBrain::VoiceHoldRecognized {
                gesture,
                thread,
                selection,
            },
        })
    }

    pub fn cancel_hold(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.hold_cancel = Some(hold.gesture);
            self.fence = self.fence.max(hold.run.unwrap_or_default());
        }
    }

    pub fn take_hold_cancel(&mut self) -> Option<ControlToBrain> {
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
