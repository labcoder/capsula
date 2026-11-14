//! First-run onboarding's plain state: which step shows and what it waits for. No GPUI, no
//! transport and no text: the names stay in [`Settings`], so nothing here can log one.

use kumochi_protocol::message::{
    AccountRecovery, AccountView, OnboardingStep, PlanState, SettingsView,
};

use crate::settings::{Error, Field, Settings};

/// The step on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// "Hi, I'm ⟨name⟩", with the assistant's name in an input.
    Welcome,
    /// What the assistant should call the user; optional.
    YourName,
    /// Connect any account, or skip it.
    Accounts,
    /// Double-tap the shortcut to finish into typing, or Finish into the peek.
    Shortcut,
}

impl Step {
    /// The name the step edits, if it has one.
    pub fn field(self) -> Option<Field> {
        match self {
            Self::Welcome => Some(Field::Assistant),
            Self::YourName => Some(Field::User),
            Self::Accounts | Self::Shortcut => None,
        }
    }

    /// The step the brain remembers once this one is completed.
    pub fn recorded(self) -> OnboardingStep {
        match self {
            Self::Welcome => OnboardingStep::Welcome,
            Self::YourName => OnboardingStep::YourName,
            Self::Accounts => OnboardingStep::Accounts,
            Self::Shortcut => OnboardingStep::Shortcut,
        }
    }

    /// Where a launch resumes: the first step not yet completed.
    fn resume(from: OnboardingStep) -> Self {
        match from {
            OnboardingStep::Welcome => Self::Welcome,
            OnboardingStep::YourName => Self::YourName,
            OnboardingStep::Accounts => Self::Accounts,
            OnboardingStep::Shortcut => Self::Shortcut,
        }
    }
}

/// A usable `ChatGPT` account: signed in, with the plan enabled and no sign-in again owed.
pub fn account_complete(view: Option<&AccountView>) -> bool {
    matches!(
        view,
        Some(AccountView::SignedIn {
            plan: PlanState::Enabled,
            recovery,
            ..
        }) if *recovery != Some(AccountRecovery::SignInAgain)
    )
}

/// What closing lands on once the brain says onboarding is finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// The finish this window's Finish asked for: the peek opens.
    Peek,
    /// The finish a double-tap asked for: the typing box opens.
    Typing,
    /// Onboarding finished without this window asking: nothing opens.
    Quiet,
}

/// Whether onboarding may still open this run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThisRun {
    #[default]
    Owed,
    /// Finish later was chosen, or the window couldn't open: not again until the next launch.
    PutOff,
}

/// What the window does after a submit or a settings change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing beyond redrawing.
    Stay,
    /// The next step shows; its input takes focus.
    Moved,
    /// The last step went on: send `FinishOnboarding`, landing here once it's answered.
    Finish(Landing),
    /// The brain says onboarding is finished: close, landing here.
    Close(Landing),
}

#[derive(Debug)]
pub struct Onboarding {
    step: Step,
    /// Go on once this field's save settles without an error.
    awaiting: Option<Field>,
    /// `FinishOnboarding` is queued and unanswered, and where to land when it's answered.
    finishing: Option<Landing>,
}

impl Onboarding {
    /// What a settings view opens, at the first step not yet completed: none once the brain says
    /// onboarding isn't owed, or once it was put off this run.
    pub fn begin(view: &SettingsView, run: ThisRun) -> Option<Self> {
        (!view.onboarded && run == ThisRun::Owed).then_some(Self {
            step: Step::resume(view.onboarding_from),
            awaiting: None,
            finishing: None,
        })
    }

    pub fn step(&self) -> Step {
        self.step
    }

    pub fn finishing(&self) -> bool {
        self.finishing.is_some()
    }

    /// Continue, Not now or Enter, after `Settings::save` ran for the step's field. What `save`
    /// returned isn't read: false means unchanged, already saving or not sent. The field decides:
    /// saving waits for the answer, a validation or offline error stays, and anything else goes
    /// on, including the brain's error left from a refusal once the draft is back at the saved
    /// name. A step with no field goes on. With no settings view, nothing happens.
    pub fn submit(&mut self, settings: &Settings) -> Next {
        if settings.view().is_none() || self.finishing.is_some() {
            return Next::Stay;
        }
        let Some(field) = self.step.field() else {
            return self.go_on();
        };
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
    pub fn settings_changed(&mut self, settings: &Settings) -> Next {
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

    /// A double-tap while the window shows is used up; at the shortcut it asks for the finish that
    /// opens typing. With no settings view, or a finish already queued, nothing happens.
    pub fn double_tap(&self, settings: &Settings) -> Next {
        if self.step == Step::Shortcut && settings.view().is_some() && self.finishing.is_none() {
            Next::Finish(Landing::Typing)
        } else {
            Next::Stay
        }
    }

    /// `FinishOnboarding` was queued; the window closes only when the brain says so.
    pub fn finish_queued(&mut self, landing: Landing) {
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
            Step::YourName => {
                self.step = Step::Accounts;
                Next::Moved
            }
            Step::Accounts => {
                self.step = Step::Shortcut;
                Next::Moved
            }
            Step::Shortcut => Next::Finish(Landing::Peek),
        }
    }
}

#[cfg(test)]
mod tests;
