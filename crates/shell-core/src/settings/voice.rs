use kumochi_protocol::message::{PartState, SttUnload, UserError};

use super::{Pending, Settings, SettingsOp, Slot};
use crate::strings::Text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechInputAction {
    Install,
    Retry,
    Reload,
    Cancel,
}

impl SpeechInputAction {
    pub fn label(self) -> Text {
        match self {
            Self::Install => Text::VoiceInstallModel,
            Self::Retry => Text::VoiceRetryModel,
            Self::Reload => Text::VoiceLoadModel,
            Self::Cancel => Text::VoiceCancelSetup,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechInputIntent {
    generation: u64,
    pub action: SpeechInputAction,
}

impl Settings {
    pub fn hold_to_talk_notice(&self) -> Option<Text> {
        if !self.hold_to_talk_enabled() {
            return Some(if self.view.is_some() {
                Text::HoldDisabled
            } else {
                Text::VoiceStatusUnknown
            });
        }
        match self.speech_input {
            Some(PartState::Ready | PartState::Loading | PartState::Unloaded) => None,
            _ => Some(self.speech_input_status()),
        }
    }

    pub(crate) fn hold_to_talk_enabled(&self) -> bool {
        self.view.as_ref().is_some_and(|view| view.hold_to_talk)
            && self.hold_to_talk_choice() != Some(false)
    }

    /// `hold_to_talk_enabled`, for the shell's tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_to_talk_enabled_for_tests(&self) -> bool {
        self.hold_to_talk_enabled()
    }

    pub fn hold_to_talk_choice(&self) -> Option<bool> {
        let saved = self.view.as_ref()?.hold_to_talk;
        self.pending
            .iter()
            .find_map(|pending| match (pending.slot, pending.failed) {
                (Slot::HoldToTalk(on), false) => Some(on),
                _ => None,
            })
            .or(Some(saved))
    }

    pub fn hold_to_talk_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.slot, Slot::HoldToTalk(_)))
    }

    pub fn hold_to_talk_error(&self) -> Option<&UserError> {
        self.notice_slot
            .filter(|slot| matches!(slot, Slot::HoldToTalk(_)))
            .and(self.notice.as_ref())
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the checkbox and wire setter carry this boolean unchanged"
    )]
    pub fn send_hold_to_talk(&mut self, on: bool, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        if self.hold_to_talk_pending()
            || self.hold_to_talk_choice().is_none_or(|old| old == on)
            || !send(SettingsOp::SetHoldToTalk(on))
        {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::HoldToTalk(on),
            failed: false,
            reset_draft: false,
        });
        true
    }

    pub fn receive_speech_input(
        &mut self,
        current: Option<u64>,
        generation: u64,
        state: PartState,
    ) -> bool {
        if current != Some(generation) {
            return false;
        }
        self.speech_input = Some(state);
        self.speech_generation = Some(generation);
        true
    }

    pub fn speech_input_intent(&self) -> Option<SpeechInputIntent> {
        self.view.as_ref()?;
        let action = match self.speech_input? {
            PartState::NotInstalled => SpeechInputAction::Install,
            PartState::Failed { .. } => SpeechInputAction::Retry,
            PartState::Unloaded => SpeechInputAction::Reload,
            PartState::Verifying | PartState::Downloading { .. } | PartState::Loading => {
                SpeechInputAction::Cancel
            }
            PartState::Ready | PartState::Unavailable => return None,
        };
        Some(SpeechInputIntent {
            generation: self.speech_generation?,
            action,
        })
    }

    pub fn speech_input_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.slot, Slot::SpeechInput(_)))
    }

    pub fn speech_input_error(&self) -> Option<&UserError> {
        self.speech_error.as_ref()
    }

    pub fn speech_input_send_failed(&self) -> bool {
        self.speech_send_failed
    }

    pub fn speech_input_failure(&self) -> Option<UserError> {
        match self.speech_input? {
            PartState::Failed { key, .. } => Some(key.into()),
            _ => None,
        }
    }

    pub fn send_speech_input(
        &mut self,
        intent: SpeechInputIntent,
        current: Option<u64>,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.speech_input_intent() != Some(intent) || self.speech_input_pending() {
            return false;
        }
        if current != Some(intent.generation) {
            self.speech_send_failed |= current.is_none();
            return false;
        }
        let op = match intent.action {
            SpeechInputAction::Install | SpeechInputAction::Retry | SpeechInputAction::Reload => {
                SettingsOp::InstallVoice(kumochi_protocol::voice::VoicePart::SpeechInput)
            }
            SpeechInputAction::Cancel => SettingsOp::CancelVoiceInstall,
        };
        if !send(op) {
            self.speech_send_failed = true;
            return false;
        }
        self.speech_send_failed = false;
        self.pending.push_back(Pending {
            slot: Slot::SpeechInput(intent.action),
            failed: false,
            reset_draft: false,
        });
        true
    }

    pub fn speech_input_status(&self) -> Text {
        match self.speech_input {
            None => Text::VoiceStatusUnknown,
            Some(PartState::NotInstalled) => Text::VoiceNotInstalled,
            Some(PartState::Verifying) => Text::VoiceVerifying,
            Some(PartState::Downloading { .. }) => Text::VoiceDownloading,
            Some(PartState::Loading) => Text::VoiceLoading,
            Some(PartState::Ready) => Text::VoiceReady,
            Some(PartState::Unloaded) => Text::VoiceUnloaded,
            Some(PartState::Unavailable) => Text::VoiceRestartRequired,
            Some(PartState::Failed {
                installed: true, ..
            }) => Text::VoiceInstalledFailed,
            Some(PartState::Failed {
                installed: false, ..
            }) => Text::VoiceNotInstalledFailed,
        }
    }

    pub fn stt_unload_choice(&self) -> Option<SttUnload> {
        let saved = self.view.as_ref()?.stt_unload;
        self.pending
            .iter()
            .find_map(|pending| match (pending.slot, pending.failed) {
                (Slot::SttUnload(policy), false) => Some(policy),
                _ => None,
            })
            .or(Some(saved))
    }

    pub fn stt_unload_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.slot, Slot::SttUnload(_)))
    }

    pub fn stt_unload_error(&self) -> Option<&UserError> {
        self.stt_error.as_ref()
    }

    pub fn stt_unload_send_failed(&self) -> bool {
        self.stt_send_failed
    }

    pub fn send_stt_unload(
        &mut self,
        policy: SttUnload,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.stt_unload_pending()
            || self
                .stt_unload_choice()
                .is_none_or(|current| current == policy)
        {
            return false;
        }
        if !send(SettingsOp::SetSttUnload(policy)) {
            self.stt_send_failed = true;
            return false;
        }
        self.stt_send_failed = false;
        self.pending.push_back(Pending {
            slot: Slot::SttUnload(policy),
            failed: false,
            reset_draft: false,
        });
        true
    }
}

#[cfg(test)]
mod tests;
