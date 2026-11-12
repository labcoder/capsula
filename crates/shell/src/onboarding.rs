//! First-run onboarding's plain state: which step shows and what it waits for. No GPUI, no
//! transport and no text: the names stay in [`Settings`], so nothing here can log one.

use kumochi_protocol::message::SettingsView;

use crate::settings::{Error, Field, Settings};

/// The step on screen. Sign-in and the shortcut come in later steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// "Hi, I'm ⟨name⟩", with the assistant's name in an input.
    Welcome,
    /// What the assistant should call the user; optional.
    YourName,
}

impl Step {
    /// The name the step edits.
    pub(crate) fn field(self) -> Field {
        match self {
            Self::Welcome => Field::Assistant,
            Self::YourName => Field::User,
        }
    }
}

/// What closing lands on once the brain says onboarding is finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// The finish this window asked for: the peek opens.
    Peek,
    /// Onboarding finished without this window asking: nothing opens.
    Quiet,
}

/// Whether onboarding may still open this run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThisRun {
    #[default]
    Owed,
    /// Finish later was chosen, or the window couldn't open: not again until the next launch.
    PutOff,
}

/// What the window does after a submit or a settings change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Nothing beyond redrawing.
    Stay,
    /// The next step shows; its input takes focus.
    Moved,
    /// The last step went on: send `FinishOnboarding`.
    Finish,
    /// The brain says onboarding is finished: close, landing here.
    Close(Landing),
}

#[derive(Debug)]
pub(crate) struct Onboarding {
    step: Step,
    /// Go on once this field's save settles without an error.
    awaiting: Option<Field>,
    /// `FinishOnboarding` is queued and unanswered, and where to land when it's answered.
    finishing: Option<Landing>,
}

impl Onboarding {
    /// What a settings view opens: none once the brain says onboarding isn't owed, or once it
    /// was put off this run.
    pub(crate) fn begin(view: &SettingsView, run: ThisRun) -> Option<Self> {
        (!view.onboarded && run == ThisRun::Owed).then_some(Self {
            step: Step::Welcome,
            awaiting: None,
            finishing: None,
        })
    }

    pub(crate) fn step(&self) -> Step {
        self.step
    }

    pub(crate) fn finishing(&self) -> bool {
        self.finishing.is_some()
    }

    /// Continue or Enter, after `Settings::save` ran for the step's field. What `save` returned
    /// isn't read: false means unchanged, already saving or not sent. The field decides: saving
    /// waits for the answer, a validation or offline error stays, and anything else goes on,
    /// including the brain's error left from a refusal once the draft is back at the saved name.
    /// With no settings view, nothing happens.
    pub(crate) fn submit(&mut self, settings: &Settings) -> Next {
        if settings.view().is_none() || self.finishing.is_some() {
            return Next::Stay;
        }
        let field = self.step.field();
        let state = settings.field(field);
        if state.saving {
            self.awaiting = Some(field);
            return Next::Stay;
        }
        if matches!(state.error, Some(Error::Validation(_) | Error::Offline)) {
            return Next::Stay;
        }
        self.go_on()
    }

    /// A settings view or answer was applied, or the link dropped. An awaited save that settled
    /// goes on when its field has no error. A finish that's no longer queued was refused, so it
    /// can be asked for again.
    pub(crate) fn settings_changed(&mut self, settings: &Settings) -> Next {
        let Some(view) = settings.view() else {
            self.disconnected();
            return Next::Stay;
        };
        if view.onboarded {
            return Next::Close(self.finishing.unwrap_or(Landing::Quiet));
        }
        if !settings.finishing() {
            self.finishing = None;
        }
        let Some(field) = self.awaiting else {
            return Next::Stay;
        };
        let state = settings.field(field);
        if state.saving {
            return Next::Stay;
        }
        self.awaiting = None;
        if state.error.is_some() {
            Next::Stay
        } else {
            self.go_on()
        }
    }

    /// `FinishOnboarding` was queued; the window closes only when the brain says so.
    pub(crate) fn finish_queued(&mut self, landing: Landing) {
        self.finishing = Some(landing);
    }

    /// The link dropped: no save or finish is awaited any more, and the step stays.
    pub(crate) fn disconnected(&mut self) {
        self.awaiting = None;
        self.finishing = None;
    }

    fn go_on(&mut self) -> Next {
        match self.step {
            Step::Welcome => {
                self.step = Step::YourName;
                Next::Moved
            }
            Step::YourName => Next::Finish,
        }
    }
}

#[cfg(test)]
mod tests;
