use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};
use kumochi_protocol::message::{
    ControlToBrain, GestureId, HeldScreen, LeaseGeneration, ModelName, RunId, RunOrigin,
    ScreenClass, ScreenHolds, ScreenOff, ThreadId,
};

use super::{Applied, Refused, Runs, Target};
use crate::strings::Text;

/// What makes a hold a screen hold: the pointer sampled at recognition, the exclusion lease
/// protecting the capture, and when the hold was recognized. It has no `Debug`, so the pointer
/// is never printed.
#[derive(Clone, Copy)]
pub struct ScreenHold {
    pub pointer: PhysicalPoint,
    pub lease: LeaseGeneration,
    pub t_qpc: u64,
}

/// What the shell took when the keys of a hold that shows the screen at release came up: the
/// pointer, its path over the hold, the exclusion lease, when the keys were released and when
/// the lease's last window was excluded. It has no `Debug`, so neither the pointer nor the
/// path is printed.
pub struct HoldRelease {
    pub pointer: PhysicalPoint,
    pub path: PointerPath,
    pub lease: LeaseGeneration,
    pub t_qpc: u64,
    pub excluded_qpc: u64,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenNotice<'a> {
    /// Nothing shows: this system can't capture a window.
    Hidden,
    /// A hold shows this model the window.
    On(&'a ModelName),
    /// The default model, named here, can't be sent images.
    ModelCantSee(&'a ModelName),
    NotReady,
    TurnedOff,
    /// The target thread runs on its own model, which no `ScreenHolds` state speaks for.
    OwnModel,
}

impl<'a> ScreenNotice<'a> {
    /// The standing icon's tooltip and the model it names; none when no icon shows.
    pub fn text(self) -> Option<(Text, Option<&'a ModelName>)> {
        Some(match self {
            Self::Hidden => return None,
            Self::On(model) => (Text::ScreenHoldsOn, Some(model)),
            Self::ModelCantSee(model) => (Text::ScreenHoldsModelCantSee, Some(model)),
            Self::NotReady => (Text::ScreenHoldsNotReady, None),
            Self::TurnedOff => (Text::ScreenHoldsTurnedOff, None),
            Self::OwnModel => (Text::ScreenHoldsOwnModel, None),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    pub(super) gesture: GestureId,
    pub(super) revision: u64,
    pub(super) thread: Option<ThreadId>,
    pub(super) run: Option<RunId>,
    /// Set for a screen hold, and for one refused its lease. Never the pointer: `Runs` prints
    /// this.
    pub(super) screen: Option<ScreenLine>,
    /// A hold that shows the screen once it's released, until its `ScreenRelease` is built or
    /// refused.
    pub(super) release_due: bool,
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
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the settings model's answer about the target thread, carried unchanged"
    )]
    pub fn screen_notice<'a>(
        &'a self,
        own_model: bool,
        default: Option<&'a ModelName>,
    ) -> ScreenNotice<'a> {
        match &self.screen_holds {
            Some(ScreenHolds::Off(ScreenOff::Unsupported)) => ScreenNotice::Hidden,
            Some(ScreenHolds::Off(ScreenOff::TurnedOff)) => ScreenNotice::TurnedOff,
            _ if own_model => ScreenNotice::OwnModel,
            Some(ScreenHolds::On { model }) if Some(model) == default => ScreenNotice::On(model),
            Some(ScreenHolds::Off(ScreenOff::ModelCantSee)) => {
                default.map_or(ScreenNotice::NotReady, ScreenNotice::ModelCantSee)
            }
            Some(ScreenHolds::On { .. } | ScreenHolds::Off(ScreenOff::NotReady)) | None => {
                ScreenNotice::NotReady
            }
        }
    }

    /// The current hold's line and the model it names. The prediction names `notice`'s model,
    /// so it shows nothing once the notice has stopped being `On`.
    pub fn hold_screen_text<'a>(
        &self,
        notice: ScreenNotice<'a>,
    ) -> Option<(Text, Option<&'a ModelName>)> {
        match (self.hold_screen_line()?, notice) {
            (ScreenLine::WillSee, ScreenNotice::On(model)) => {
                Some((Text::ScreenHoldWillSee, Some(model)))
            }
            (ScreenLine::WillSee, _) => None,
            (ScreenLine::NotIncluded(class), _) => Some((Text::screen_not_included(class), None)),
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
            HeldScreen::Attached | HeldScreen::Withdrawn | HeldScreen::Screen => None,
        };
        true
    }

    /// The current hold was to be a screen hold and was refused its capture lease, so it went
    /// on voice only: its line says the screen wasn't ready, as a lease that lapses before its
    /// frame does.
    pub fn hold_screen_refused(&mut self) {
        if let Some(hold) = &mut self.hold {
            hold.screen = Some(ScreenLine::NotIncluded(ScreenClass::NotReady));
        }
    }

    /// Admits a hold and makes it the current one: a voice-only hold until its caller says
    /// otherwise.
    fn admit_hold(&mut self, gesture: GestureId) -> Result<&mut Hold, Refused> {
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
        Ok(self.hold.insert(Hold {
            gesture,
            revision: self.revision,
            thread,
            run: None,
            screen: None,
            release_due: false,
        }))
    }

    /// Starts a hold: a screen hold when `screen` is given, else a voice-only one.
    pub fn begin_hold(
        &mut self,
        gesture: GestureId,
        screen: Option<ScreenHold>,
    ) -> Result<ControlToBrain, Refused> {
        let hold = self.admit_hold(gesture)?;
        hold.screen = screen.map(|_| ScreenLine::WillSee);
        let (thread, selection) = (hold.thread, hold.revision);
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

    /// Starts a hold that shows the model the screen once it's released, while the standing
    /// notice is `On`; else a voice-only one. The notice is [`Self::screen_notice`]'s, read here
    /// from the same two answers. Nothing of the screen is taken now, and the hold has no line
    /// of its own yet.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the settings model's answer about the target thread, carried unchanged"
    )]
    pub fn begin_screen_hold(
        &mut self,
        gesture: GestureId,
        t_qpc: u64,
        own_model: bool,
        default: Option<&ModelName>,
    ) -> Result<ControlToBrain, Refused> {
        let on = matches!(self.screen_notice(own_model, default), ScreenNotice::On(_));
        let hold = self.admit_hold(gesture)?;
        hold.release_due = on;
        let (thread, selection) = (hold.thread, hold.revision);
        Ok(if on {
            ControlToBrain::ScreenHoldRecognized {
                gesture,
                thread,
                selection,
                t_qpc,
            }
        } else {
            ControlToBrain::VoiceHoldRecognized {
                gesture,
                thread,
                selection,
            }
        })
    }

    /// Whether `gesture`'s release shows the screen: it is the current hold, it began as one
    /// that shows the screen at release, no `ScreenRelease` was built or refused for it, and the
    /// standing notice, read now as [`Self::screen_notice`] reads it, is still `On`. A no for
    /// the current hold is final: nothing builds its release later, whatever the notice says
    /// then. A cancelled hold is no longer current, and a release before the hold is listening
    /// cancels it. The shell asks before it takes the lease.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the settings model's answer about the target thread, carried unchanged"
    )]
    pub fn releases_screen(
        &mut self,
        gesture: GestureId,
        own_model: bool,
        default: Option<&ModelName>,
    ) -> bool {
        let on = matches!(self.screen_notice(own_model, default), ScreenNotice::On(_));
        let Some(hold) = self.hold.as_mut().filter(|hold| hold.gesture == gesture) else {
            return false;
        };
        hold.release_due &= on;
        hold.release_due
    }

    /// Builds the one `ScreenRelease` of `gesture`'s hold, by the rule of
    /// [`Self::releases_screen`], which it asks again. None leaves the lease unused.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the settings model's answer about the target thread, carried unchanged"
    )]
    pub fn screen_release(
        &mut self,
        gesture: GestureId,
        own_model: bool,
        default: Option<&ModelName>,
        release: HoldRelease,
    ) -> Option<ControlToBrain> {
        if !self.releases_screen(gesture, own_model, default) {
            return None;
        }
        if let Some(hold) = &mut self.hold {
            hold.release_due = false;
        }
        let HoldRelease {
            pointer,
            path,
            lease,
            t_qpc,
            excluded_qpc,
        } = release;
        Some(ControlToBrain::ScreenRelease {
            gesture,
            lease,
            t_qpc,
            excluded_qpc,
            pointer: ReleasePointer(pointer),
            path,
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
