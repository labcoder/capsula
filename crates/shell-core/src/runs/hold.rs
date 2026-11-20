use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};
use kumochi_protocol::message::{
    ControlToBrain, GestureId, HeldScreen, LeaseGeneration, ModelName, NoticeRevision, RunId,
    RunOrigin, ScreenClass, ScreenHolds, ScreenOff, ThreadId,
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
    /// The window flow's prediction, from recognition until the brain says what the turn
    /// carried. A hold that shows the screen at release predicts nothing.
    WillSee,
    NotIncluded(ScreenClass),
    /// The user turned the setting off while the keys were down.
    TurnedOff,
}

/// The standing notice: what a hold on the current target does with the screen, and the only
/// model it may name. A hold shows the screen only while this is `On`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenNotice<'a> {
    /// This system can't capture.
    Hidden,
    /// A hold sends this model the screen's picture.
    On(&'a ModelName),
    /// The default model, named here, can't be sent images.
    ModelCantSee(&'a ModelName),
    NotReady,
    TurnedOff,
    /// The target thread runs on its own model, which no `ScreenHolds` state speaks for.
    OwnModel,
}

impl ScreenNotice<'_> {
    /// What a hold's line says when this notice refuses its release; none while it is `On`,
    /// and none where this system can't capture.
    const fn refusal(self) -> Option<ScreenLine> {
        match self {
            Self::On(_) | Self::Hidden => None,
            Self::TurnedOff => Some(ScreenLine::TurnedOff),
            Self::ModelCantSee(_) => Some(ScreenLine::NotIncluded(ScreenClass::ModelCantSee)),
            Self::NotReady | Self::OwnModel => Some(ScreenLine::NotIncluded(ScreenClass::NotReady)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    pub(super) gesture: GestureId,
    pub(super) revision: u64,
    pub(super) thread: Option<ThreadId>,
    pub(super) run: Option<RunId>,
    /// Set for a window hold, and for a hold whose screen was refused. Never the pointer:
    /// `Runs` prints this.
    pub(super) screen: Option<ScreenLine>,
    /// A hold that began as one that shows the screen at release. It keeps that kind for life.
    pub(super) at_release: bool,
    /// A hold that shows the screen once it's released, until its `ScreenRelease` is built or
    /// refused.
    pub(super) release_due: bool,
    /// The brain's word about this hold's screen has landed; a second one doesn't.
    pub(super) told: bool,
    /// The offer its first input was seen under, for a hold that shows the screen at release.
    pub(super) offer: Option<NoticeRevision>,
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
            Some(ScreenHolds::On { model, .. }) if Some(model) == default => {
                ScreenNotice::On(model)
            }
            Some(ScreenHolds::Off(ScreenOff::ModelCantSee)) => {
                default.map_or(ScreenNotice::NotReady, ScreenNotice::ModelCantSee)
            }
            Some(ScreenHolds::On { .. } | ScreenHolds::Off(ScreenOff::NotReady)) | None => {
                ScreenNotice::NotReady
            }
        }
    }

    /// The current hold's line and the model it names. The prediction names `notice`'s model,
    /// so it shows nothing once the notice has stopped being `On`. A hold that shows the screen
    /// at release says "too large" and "changed" in that flow's own words.
    pub fn hold_screen_text<'a>(
        &self,
        notice: ScreenNotice<'a>,
    ) -> Option<(Text, Option<&'a ModelName>)> {
        let hold = self.hold?;
        let text = match (hold.screen?, notice) {
            (ScreenLine::WillSee, ScreenNotice::On(model)) => {
                return Some((Text::ScreenHoldWillSee, Some(model)));
            }
            (ScreenLine::WillSee, _) => return None,
            (ScreenLine::TurnedOff, _) => Text::ScreenNotIncludedTurnedOff,
            (ScreenLine::NotIncluded(ScreenClass::TooLarge), _) if hold.at_release => {
                Text::ScreenNotIncludedPictureTooLarge
            }
            (ScreenLine::NotIncluded(ScreenClass::Changed), _) if hold.at_release => {
                Text::ShowScreenChanged
            }
            (ScreenLine::NotIncluded(class), _) => Text::screen_not_included(class),
        };
        Some((text, None))
    }

    /// The current hold's line about the screen; it goes when the hold does.
    pub fn hold_screen_line(&self) -> Option<ScreenLine> {
        self.hold.and_then(|hold| hold.screen)
    }

    /// The brain said what `gesture`'s hold carried into its turn. False, and nothing changes,
    /// unless that is the current hold and its line is still open: the window flow's prediction,
    /// or no line yet on a hold that shows the screen at release.
    pub fn hold_screen(&mut self, gesture: GestureId, outcome: HeldScreen) -> bool {
        let Some(hold) = self.hold.as_mut().filter(|hold| {
            hold.gesture == gesture
                && match hold.screen {
                    Some(ScreenLine::WillSee) => true,
                    None => hold.at_release && !hold.told,
                    Some(ScreenLine::NotIncluded(_) | ScreenLine::TurnedOff) => false,
                }
        }) else {
            return false;
        };
        hold.told = true;
        hold.screen = match outcome {
            HeldScreen::NotIncluded(class) => Some(ScreenLine::NotIncluded(class)),
            // History carries these.
            HeldScreen::Attached | HeldScreen::Withdrawn | HeldScreen::Screen => None,
        };
        true
    }

    /// The current hold was to show the screen and couldn't take what its capture needs (the
    /// lease, or the pointer at release), so it went on voice only: its line says the screen
    /// wasn't ready, as a lease that lapses before its frame does.
    pub fn hold_screen_refused(&mut self, gesture: GestureId) {
        if let Some(hold) = self.hold.as_mut().filter(|hold| hold.gesture == gesture) {
            hold.screen = Some(ScreenLine::NotIncluded(ScreenClass::NotReady));
            hold.release_due = false;
        }
    }

    /// The current hold was to show the screen and the user has switched that off, though the
    /// brain hasn't answered yet: it goes on voice only, and its line says so.
    pub fn hold_screen_turned_off(&mut self) {
        if let Some(hold) = &mut self.hold {
            hold.screen = Some(ScreenLine::TurnedOff);
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
            at_release: false,
            release_due: false,
            told: false,
            offer: None,
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
                notice: None,
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
    /// from the same two answers, and `offer` is the one its first input was seen under: without
    /// one it's voice only. Nothing of the screen is taken now, and the hold has no line of its
    /// own yet.
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
        offer: Option<NoticeRevision>,
    ) -> Result<ControlToBrain, Refused> {
        let on = offer.is_some()
            && matches!(self.screen_notice(own_model, default), ScreenNotice::On(_));
        let hold = self.admit_hold(gesture)?;
        hold.at_release = on;
        hold.release_due = on;
        hold.offer = offer.filter(|_| on);
        let (thread, selection) = (hold.thread, hold.revision);
        Ok(if on {
            ControlToBrain::ScreenHoldRecognized {
                gesture,
                thread,
                selection,
                t_qpc,
                notice: offer,
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
    /// then, and the hold's line says why the notice refused it. A cancelled hold is no longer
    /// current, and a release before the hold is listening cancels it. The shell asks before it
    /// takes the lease.
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
        let notice = self.screen_notice(own_model, default);
        let (on, refusal) = (matches!(notice, ScreenNotice::On(_)), notice.refusal());
        let Some(hold) = self.hold.as_mut().filter(|hold| hold.gesture == gesture) else {
            return false;
        };
        if hold.release_due && !on {
            hold.screen = refusal;
        }
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
        let notice = self.hold.as_mut().and_then(|hold| {
            hold.release_due = false;
            hold.offer
        });
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
            notice,
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
