//! Settings drafts and ordered save answers, independent of the UI and transport.

use std::collections::VecDeque;
use std::fmt;

use kumochi_protocol::message::{
    AssistantName, ConnectionKind, DockPosition, ErrorKey, MaxFrameRate, ModelChoice, ModelInfo,
    NameError, OnboardingStep, SettingsOp, SettingsView, SttUnload, UserError, UserName, VoiceView,
};
use kumochi_protocol::voice::VoicePart;

use crate::strings::{Strings, Text};

mod dock;
mod model;
mod options;
mod saves;
mod voice;
pub use saves::{AfterSave, BeforeSave, Save, SaveOutcome};
use voice::VoiceFeedback;
pub use voice::{STT_UNLOAD_OPTIONS, VoiceAction, VoiceIntent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Assistant,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    Answer,
    Unsolicited,
    SpokenRepliesDefaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskbarChoice {
    Hidden,
    Shown,
}

#[derive(Clone, Copy, Debug)]
enum Draft {
    Reset,
    Preserve,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Validation(NameError),
    #[error("the brain refused the settings change")]
    Brain(UserError),
    #[error("the brain is offline")]
    Offline,
}

#[derive(Default)]
pub struct FieldState {
    pub saving: bool,
    pub error: Option<Error>,
    value: String,
    authoritative: String,
}

impl fmt::Debug for FieldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldState")
            .field("saving", &self.saving)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    pub(crate) failed: bool,
    pub(crate) field: Field,
}

/// What a queued settings operation changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Name(Field),
    /// The frame rate has no draft; the rate is kept so a quick second choice compares to it.
    FrameRate(MaxFrameRate),
    Dock(DockPosition),
    Taskbar(TaskbarChoice),
    SttUnload(SttUnload),
    HoldToTalk(bool),
    TalkBack(bool),
    ShowScreen(bool),
    Voice(VoicePart, VoiceAction),
    /// The default model; the choice itself is `Settings::model_queued`.
    Model,
    /// `FinishOnboarding`: no field and no draft.
    Onboarding,
    /// `CompleteOnboardingStep`: no field, and never counted as the finish.
    OnboardingStep,
}

impl Slot {
    fn same_setting(self, other: Self) -> bool {
        match (self, other) {
            (Self::Voice(left, _), Self::Voice(right, _)) => left == right,
            (Self::Name(one), Self::Name(other)) => one == other,
            (Self::FrameRate(_), Self::FrameRate(_))
            | (Self::Dock(_), Self::Dock(_))
            | (Self::Taskbar(_), Self::Taskbar(_))
            | (Self::SttUnload(_), Self::SttUnload(_))
            | (Self::HoldToTalk(_), Self::HoldToTalk(_))
            | (Self::TalkBack(_), Self::TalkBack(_))
            | (Self::ShowScreen(_), Self::ShowScreen(_))
            | (Self::Model, Self::Model)
            | (Self::Onboarding, Self::Onboarding)
            | (Self::OnboardingStep, Self::OnboardingStep) => true,
            _ => false,
        }
    }

    fn field(self) -> Option<Field> {
        match self {
            Self::Name(field) => Some(field),
            Self::FrameRate(_)
            | Self::Dock(_)
            | Self::Taskbar(_)
            | Self::SttUnload(_)
            | Self::HoldToTalk(_)
            | Self::TalkBack(_)
            | Self::ShowScreen(_)
            | Self::Voice(..)
            | Self::Model
            | Self::Onboarding
            | Self::OnboardingStep => None,
        }
    }
}

#[derive(Debug)]
struct Pending {
    slot: Slot,
    failed: bool,
    reset_draft: bool,
}

#[derive(Clone, Copy, Debug)]
enum DockIntent {
    Unsent(DockPosition),
    Replay(DockPosition),
}

impl DockIntent {
    fn position(self) -> DockPosition {
        match self {
            Self::Unsent(position) | Self::Replay(position) => position,
        }
    }
}

#[derive(Default)]
pub struct Settings {
    pub accounts: crate::accounts::Accounts,
    pub claude: crate::claude_accounts::ClaudeAccounts,
    pub github: crate::github_connections::GitHubConnections,
    pub key_paste: crate::key_paste::KeyPaste,
    pub native_taskbar_notice: Option<Text>,
    view: Option<SettingsView>,
    view_generation: Option<u64>,
    notice: Option<UserError>,
    notice_slot: Option<Slot>,
    assistant: FieldState,
    user: FieldState,
    pending: VecDeque<Pending>,
    offline_position: Option<DockIntent>,
    voice: Option<(u64, VoiceView)>,
    voice_feedback: [VoiceFeedback; 2],
    stt_error: Option<UserError>,
    stt_send_failed: bool,
    talk_back_send_error: Option<Text>,
    talk_back_deferred: bool,
    /// Each catalog on offer, in picker order.
    models: Vec<(ConnectionKind, Vec<ModelInfo>)>,
    /// The newest default model sent and not yet answered.
    model_queued: Option<ModelChoice>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("online", &self.view.is_some())
            .field("assistant", &self.assistant)
            .field("user", &self.user)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Settings {
    pub fn receive(
        &mut self,
        current: Option<u64>,
        generation: u64,
        view: SettingsView,
        update: Update,
    ) -> bool {
        if current != Some(generation) {
            return false;
        }
        for field in [Field::Assistant, Field::User] {
            let state = self.field_mut(field);
            if state.error == Some(Error::Offline) {
                state.error = None;
            }
        }
        self.view_generation = Some(generation);
        let _completion = self.set_view(view, update);
        true
    }

    pub fn set_view(&mut self, view: SettingsView, update: Update) -> Option<Completion> {
        let sync_assistant = !self.assistant.saving && !self.editing(Field::Assistant);
        let sync_user = !self.user.saving && !self.editing(Field::User);
        view.assistant_name
            .as_str()
            .clone_into(&mut self.assistant.authoritative);
        view.user_name
            .as_ref()
            .map_or("", UserName::as_str)
            .clone_into(&mut self.user.authoritative);
        if sync_assistant {
            self.assistant
                .value
                .clone_from(&self.assistant.authoritative);
        }
        if sync_user {
            self.user.value.clone_from(&self.user.authoritative);
        }
        if view.hold_to_talk {
            let feedback = &mut self.voice_feedback[voice::part_index(VoicePart::SpeechInput)];
            if feedback.error.as_ref().map(UserError::key) == Some(ErrorKey::HoldToTalkOff) {
                feedback.error = None;
            }
            if self.notice_slot == Some(Slot::Voice(VoicePart::SpeechInput, VoiceAction::Reload))
                && self.notice.as_ref().map(UserError::key) == Some(ErrorKey::HoldToTalkOff)
            {
                self.clear_notice();
            }
        }
        self.talk_back_deferred =
            view.talk_back && (self.talk_back_deferred || update == Update::SpokenRepliesDefaulted);
        self.view = Some(view);
        if update != Update::Answer {
            return None;
        }
        let pending = self.pending.pop_front()?;
        if !pending.failed && matches!(pending.slot, Slot::SttUnload(_)) {
            self.stt_error = None;
        }
        if !pending.failed
            && let Slot::Voice(part, _) = pending.slot
        {
            self.voice_feedback[voice::part_index(part)].error = None;
        }
        if pending.slot == Slot::Model && !self.pending.iter().any(|left| left.slot == Slot::Model)
        {
            self.model_queued = None;
        }
        if !pending.failed
            && self
                .notice_slot
                .is_some_and(|slot| slot.same_setting(pending.slot))
        {
            self.clear_notice();
        }
        let field = pending.slot.field()?;
        let state = self.field_mut(field);
        state.saving = false;
        if pending.reset_draft {
            state.value.clone_from(&state.authoritative);
        }
        if !pending.failed && (pending.reset_draft || matches!(state.error, Some(Error::Brain(_))))
        {
            state.error = None;
        }
        Some(Completion {
            failed: pending.failed,
            field,
        })
    }

    pub fn view(&self) -> Option<&SettingsView> {
        self.view.as_ref()
    }

    /// The link generation that supplied the current settings view.
    pub fn view_generation(&self) -> Option<u64> {
        self.view_generation
    }

    pub fn notice(&self) -> Option<&UserError> {
        self.notice.as_ref()
    }

    pub fn dock_notice(&self) -> Option<Text> {
        if [&self.assistant, &self.user]
            .iter()
            .any(|state| matches!(state.error, Some(Error::Validation(_) | Error::Offline)))
        {
            return Some(Text::SettingsNotSaved);
        }
        self.notice.as_ref().map(|error| {
            if settings_error(error.key()) {
                Text::DockError
            } else {
                Text::DockBrainError
            }
        })
    }

    pub fn clear_notice(&mut self) {
        self.notice = None;
        self.notice_slot = None;
    }

    pub fn field(&self, field: Field) -> &FieldState {
        match field {
            Field::Assistant => &self.assistant,
            Field::User => &self.user,
        }
    }

    /// Saving and error lines under either name, using the same text in Settings and onboarding.
    pub fn field_status(
        &self,
        field: Field,
        strings: &Strings,
    ) -> (Option<String>, Option<String>) {
        let state = self.field(field);
        let error = state.error.as_ref().map(|error| match error {
            Error::Validation(NameError::Empty) => strings.get(Text::RenameEmpty),
            Error::Validation(NameError::TooLong) => strings.get(Text::RenameNameTooLong),
            Error::Validation(NameError::NotOneLine) => strings.get(Text::RenameOneLine),
            Error::Brain(error) => strings.error(error),
            Error::Offline => strings.get(if self.view().is_none() {
                Text::SettingsOffline
            } else {
                Text::SettingsUnavailable
            }),
        });
        (
            state.saving.then(|| strings.get(Text::SettingsSaving)),
            error,
        )
    }

    fn field_mut(&mut self, field: Field) -> &mut FieldState {
        match field {
            Field::Assistant => &mut self.assistant,
            Field::User => &mut self.user,
        }
    }

    pub fn value(&self, field: Field) -> &str {
        &self.field(field).value
    }

    pub fn editing(&self, field: Field) -> bool {
        let state = self.field(field);
        state.value != state.authoritative
    }

    pub fn change(&mut self, field: Field, value: &str) {
        let state = self.field_mut(field);
        if state.value == value {
            return;
        }
        value.clone_into(&mut state.value);
        state.error = None;
        for pending in &mut self.pending {
            if pending.slot == Slot::Name(field) {
                pending.reset_draft = false;
            }
        }
    }

    pub fn cancel(&mut self, field: Field) {
        if self.field(field).saving {
            return;
        }
        let value = self.field(field).authoritative.clone();
        self.change(field, &value);
    }

    pub fn save(&mut self, field: Field, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        if self.field(field).saving {
            return false;
        }
        let value = self.value(field);
        let op = match field {
            Field::Assistant => AssistantName::new(value).map(SettingsOp::SetAssistantName),
            Field::User if value.trim().is_empty() => Ok(SettingsOp::SetUserName(None)),
            Field::User => UserName::new(value).map(|name| SettingsOp::SetUserName(Some(name))),
        };
        match op {
            Ok(_) if value.trim() == self.field(field).authoritative => {
                let state = self.field_mut(field);
                state.value.clone_from(&state.authoritative);
                false
            }
            Ok(op) => {
                let queued = self.send(field, op, Draft::Reset, send);
                if !queued {
                    self.field_mut(field).error = Some(Error::Offline);
                }
                queued
            }
            Err(error) => {
                self.field_mut(field).error = Some(Error::Validation(error));
                false
            }
        }
    }

    /// Inline Kumo renames share the settings FIFO without replacing an unrelated draft.
    pub fn send_assistant(
        &mut self,
        name: AssistantName,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if name.as_str() == self.assistant.authoritative {
            return false;
        }
        self.send(
            Field::Assistant,
            SettingsOp::SetAssistantName(name),
            if self.editing(Field::Assistant) {
                Draft::Preserve
            } else {
                Draft::Reset
            },
            send,
        )
    }

    /// The newest frame rate cap the user chose: the last one queued, else the saved one.
    pub fn max_frame_rate_choice(&self) -> Option<MaxFrameRate> {
        let queued = self
            .pending
            .iter()
            .rev()
            .find_map(|pending| match pending.slot {
                Slot::FrameRate(rate) => Some(rate),
                Slot::Name(_)
                | Slot::Dock(_)
                | Slot::Taskbar(_)
                | Slot::SttUnload(_)
                | Slot::HoldToTalk(_)
                | Slot::TalkBack(_)
                | Slot::ShowScreen(_)
                | Slot::Voice(..)
                | Slot::Model
                | Slot::Onboarding
                | Slot::OnboardingStep => None,
            });
        queued.or(self.view.as_ref().map(|view| view.max_frame_rate))
    }

    /// Sends a new frame rate cap unless it's the newest one, queued or saved. It shows once the
    /// brain answers.
    pub fn send_max_frame_rate(
        &mut self,
        rate: MaxFrameRate,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self
            .max_frame_rate_choice()
            .is_none_or(|newest| newest == rate)
            || !send(SettingsOp::SetMaxFrameRate(rate))
        {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::FrameRate(rate),
            failed: false,
            reset_draft: false,
        });
        true
    }

    pub fn taskbar_choice(&self) -> Option<TaskbarChoice> {
        let view = self.view.as_ref()?;
        self.pending
            .iter()
            .rev()
            .find_map(|pending| match (pending.slot, pending.failed) {
                (Slot::Taskbar(choice), false) => Some(choice),
                _ => None,
            })
            .or(Some(if view.show_in_taskbar {
                TaskbarChoice::Shown
            } else {
                TaskbarChoice::Hidden
            }))
    }

    pub fn apply_taskbar(&mut self, apply: impl FnOnce(TaskbarChoice) -> bool) {
        if let Some(choice) = self.taskbar_choice() {
            self.native_taskbar_notice = (!apply(choice)).then_some(match choice {
                TaskbarChoice::Shown => Text::TaskbarAddFailed,
                TaskbarChoice::Hidden => Text::TaskbarRemoveFailed,
            });
        }
    }

    pub fn send_taskbar(
        &mut self,
        choice: TaskbarChoice,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> Result<bool, Text> {
        let Some(newest) = self.taskbar_choice() else {
            return Err(Text::SettingsUnavailable);
        };
        if newest == choice {
            return Ok(false);
        }
        if !send(SettingsOp::SetTaskbar(choice == TaskbarChoice::Shown)) {
            return Err(Text::SettingsUnavailable);
        }
        self.pending.push_back(Pending {
            slot: Slot::Taskbar(choice),
            failed: false,
            reset_draft: false,
        });
        Ok(true)
    }

    /// Queues `FinishOnboarding` in the ordered answer queue, so the brain's one answer to it
    /// completes it and no other change. False when offline, when one is already queued, or when
    /// the send fails.
    pub fn finish_onboarding(&mut self, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        if self.view.is_none() || self.finishing() || !send(SettingsOp::FinishOnboarding) {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::Onboarding,
            failed: false,
            reset_draft: false,
        });
        true
    }

    /// Queues `CompleteOnboardingStep` in the ordered answer queue, apart from the finish. False
    /// when offline or the send fails.
    pub fn complete_onboarding_step(
        &mut self,
        step: OnboardingStep,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.view.is_none() || !send(SettingsOp::CompleteOnboardingStep(step)) {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::OnboardingStep,
            failed: false,
            reset_draft: false,
        });
        true
    }

    /// Whether a `FinishOnboarding` is queued and unanswered.
    pub(crate) fn finishing(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.slot == Slot::Onboarding)
    }

    /// The brain's refusal of the last `FinishOnboarding`, until another change clears it.
    pub fn finish_refusal(&self) -> Option<&UserError> {
        self.notice
            .as_ref()
            .filter(|_| self.notice_slot == Some(Slot::Onboarding))
    }

    pub fn rename_refusal(&self) -> Text {
        if self.assistant.saving {
            Text::RenameBusy
        } else if self.view.is_none() {
            Text::TurnOffline
        } else {
            Text::SettingsUnavailable
        }
    }

    fn send(
        &mut self,
        field: Field,
        op: SettingsOp,
        draft: Draft,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.field(field).saving {
            return false;
        }
        if self.view.is_none() || !send(op) {
            return false;
        }
        self.field_mut(field).saving = true;
        self.pending.push_back(Pending {
            slot: Slot::Name(field),
            failed: false,
            reset_draft: matches!(draft, Draft::Reset),
        });
        true
    }

    pub fn error(&mut self, error: UserError) -> Option<Field> {
        // A step that isn't remembered only shows again next launch, so it raises no notice.
        if settings_error(error.key())
            && let Some(pending) = self
                .pending
                .front_mut()
                .filter(|pending| pending.slot == Slot::OnboardingStep)
        {
            pending.failed = true;
            return None;
        }
        self.notice = Some(error.clone());
        self.notice_slot = self
            .pending
            .front()
            .filter(|_| settings_error(error.key()))
            .map(|pending| pending.slot);
        self.notice_slot?;
        let pending = self.pending.front_mut()?;
        pending.failed = true;
        if matches!(pending.slot, Slot::SttUnload(_)) {
            self.stt_error = Some(error.clone());
        }
        if let Slot::Voice(part, _) = pending.slot {
            self.voice_feedback[voice::part_index(part)].error = Some(error.clone());
        }
        let field = pending.slot.field()?;
        self.field_mut(field).error = Some(Error::Brain(error));
        Some(field)
    }

    pub fn disconnected(&mut self) {
        if self.offline_position.is_none() {
            self.offline_position = self.pending.iter().rev().find_map(|pending| {
                match (pending.slot, pending.failed) {
                    (Slot::Dock(position), false) => Some(DockIntent::Replay(position)),
                    _ => None,
                }
            });
        }
        self.view = None;
        self.view_generation = None;
        self.voice = None;
        self.voice_feedback = Default::default();
        self.stt_send_failed = false;
        self.talk_back_send_error = None;
        self.talk_back_deferred = false;
        self.pending.clear();
        self.models.clear();
        self.model_queued = None;
        self.assistant.saving = false;
        self.user.saving = false;
    }
}

fn settings_error(key: ErrorKey) -> bool {
    matches!(
        key,
        ErrorKey::SettingsRefused
            | ErrorKey::ConfigUnreadable
            | ErrorKey::VoiceUnavailable
            | ErrorKey::HoldToTalkOff
    )
}

#[cfg(test)]
mod position_tests;

#[cfg(test)]
mod taskbar_tests;

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod followup_tests;

#[cfg(test)]
mod onboarding_tests;

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod tests;
