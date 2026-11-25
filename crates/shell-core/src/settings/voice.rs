use kumochi_protocol::message::{ErrorKey, PartState, SttUnload, UserError, VoiceView};
use kumochi_protocol::voice::VoicePart::{self, SpeechInput, SpokenReplies};

use super::{Pending, Settings, SettingsOp, Slot};
use crate::link::output::Denied;
use crate::strings::Text;

pub const STT_UNLOAD_OPTIONS: [(SttUnload, Text); 3] = [
    (SttUnload::Kept, Text::VoiceKept),
    (SttUnload::After15Min, Text::Voice15Minutes),
    (SttUnload::After1Hour, Text::Voice1Hour),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceAction {
    Install,
    Retry,
    Reload,
    Cancel,
}

impl VoiceIntent {
    pub fn label(self) -> Text {
        match (self.part, self.action) {
            (SpokenReplies, VoiceAction::Install) => Text::SpokenRepliesInstall,
            (SpokenReplies, VoiceAction::Retry) => Text::SpokenRepliesRetry,
            (SpeechInput, VoiceAction::Install) => Text::VoiceInstallModel,
            (SpeechInput, VoiceAction::Retry) => Text::VoiceRetryModel,
            (_, VoiceAction::Reload) => Text::VoiceLoadModel,
            (_, VoiceAction::Cancel) => Text::VoiceCancelSetup,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceIntent {
    generation: u64,
    pub(super) part: VoicePart,
    pub action: VoiceAction,
}

#[derive(Default)]
pub(super) struct VoiceFeedback {
    pub(super) error: Option<UserError>,
    send_failed: bool,
}

pub(super) const fn part_index(part: VoicePart) -> usize {
    match part {
        SpeechInput => 0,
        SpokenReplies => 1,
    }
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
        match self.voice_part_state(SpeechInput) {
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

    pub fn talk_back_choice(&self) -> Option<bool> {
        let saved = self.view.as_ref()?.talk_back;
        self.pending
            .iter()
            .find_map(|pending| match (pending.slot, pending.failed) {
                (Slot::TalkBack(on), false) => Some(on),
                _ => None,
            })
            .or(Some(saved))
    }

    pub fn talk_back_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.slot, Slot::TalkBack(_)))
    }

    pub fn talk_back_error(&self) -> Option<&UserError> {
        self.notice_slot
            .filter(|slot| matches!(slot, Slot::TalkBack(_)))
            .and(self.notice.as_ref())
    }

    pub fn talk_back_send_error(&self) -> Option<Text> {
        self.talk_back_send_error
    }

    pub fn talk_back_restart_needed(&self) -> bool {
        self.talk_back_deferred && self.voice_part_state(SpokenReplies) == Some(PartState::Ready)
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the checkbox and wire setter carry this boolean unchanged"
    )]
    pub fn send_talk_back(&mut self, on: bool, send: impl FnOnce() -> Result<(), Denied>) -> bool {
        if self.talk_back_pending() || self.talk_back_choice().is_none_or(|old| old == on) {
            return false;
        }
        self.talk_back_send_error = send().err().map(|denied| match denied {
            Denied::Busy => Text::TalkBackBusy,
            _ => Text::SettingsUnavailable,
        });
        if self.talk_back_send_error.is_some() {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::TalkBack(on),
            failed: false,
            reset_draft: false,
        });
        true
    }

    /// Whether a hold also sends the model the screen: the choice in flight, else the saved one.
    pub fn show_screen_choice(&self) -> Option<bool> {
        let saved = self.view.as_ref()?.show_screen;
        self.pending
            .iter()
            .find_map(|pending| match (pending.slot, pending.failed) {
                (Slot::ShowScreen(on), false) => Some(on),
                _ => None,
            })
            .or(Some(saved))
    }

    pub fn show_screen_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| matches!(pending.slot, Slot::ShowScreen(_)))
    }

    pub fn show_screen_error(&self) -> Option<&UserError> {
        self.notice_slot
            .filter(|slot| matches!(slot, Slot::ShowScreen(_)))
            .and(self.notice.as_ref())
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the checkbox and wire setter carry this boolean unchanged"
    )]
    pub fn send_show_screen(&mut self, on: bool, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        if self.show_screen_pending()
            || self.show_screen_choice().is_none_or(|old| old == on)
            || !send(SettingsOp::SetShowScreen(on))
        {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::ShowScreen(on),
            failed: false,
            reset_draft: false,
        });
        true
    }

    pub fn receive_voice(
        &mut self,
        current: Option<u64>,
        generation: u64,
        view: VoiceView,
    ) -> bool {
        if current != Some(generation) {
            return false;
        }
        self.voice = Some((generation, view));
        true
    }

    fn voice_part_state(&self, part: VoicePart) -> Option<PartState> {
        let (_, view) = self.voice?;
        Some(match part {
            SpeechInput => view.speech_input,
            SpokenReplies => view.spoken_replies,
        })
    }

    pub fn voice_setup_intent(&self, part: VoicePart) -> Option<VoiceIntent> {
        self.view.as_ref()?;
        let action = match self.voice_part_state(part)? {
            PartState::NotInstalled => VoiceAction::Install,
            PartState::Unloaded if part == SpeechInput => VoiceAction::Reload,
            PartState::Failed {
                key: ErrorKey::UnsupportedPlatform | ErrorKey::VoiceUnavailable,
                ..
            }
            | PartState::Ready
            | PartState::Unloaded
            | PartState::Unavailable => return None,
            PartState::Failed { .. } => VoiceAction::Retry,
            PartState::Verifying | PartState::Downloading { .. } | PartState::Loading => {
                VoiceAction::Cancel
            }
        };
        Some(VoiceIntent {
            generation: self.voice?.0,
            part,
            action,
        })
    }

    pub fn voice_setup_pending(&self, part: VoicePart) -> bool {
        self.pending.iter().any(|pending| {
            matches!(pending.slot, Slot::Voice(queued, action)
                if queued == part || action == VoiceAction::Cancel)
        })
    }

    pub fn voice_setup_error(&self, part: VoicePart) -> Option<&UserError> {
        self.voice_feedback[part_index(part)].error.as_ref()
    }

    pub fn voice_setup_send_failed(&self, part: VoicePart) -> bool {
        self.voice_feedback[part_index(part)].send_failed
    }

    pub fn voice_setup_failure(&self, part: VoicePart) -> Option<UserError> {
        match self.voice_part_state(part)? {
            PartState::Failed {
                key: ErrorKey::VoiceUnavailable,
                ..
            } => None,
            PartState::Failed { key, .. } => Some(key.into()),
            _ => None,
        }
    }

    pub fn send_voice_setup(
        &mut self,
        intent: VoiceIntent,
        current: Option<u64>,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        if self.voice_setup_intent(intent.part) != Some(intent)
            || self.voice_setup_pending(intent.part)
        {
            return false;
        }
        let feedback = &mut self.voice_feedback[part_index(intent.part)];
        if current != Some(intent.generation) {
            feedback.send_failed |= current.is_none();
            return false;
        }
        let op = match intent.action {
            VoiceAction::Install | VoiceAction::Retry => SettingsOp::InstallVoice(intent.part),
            VoiceAction::Reload => SettingsOp::LoadSpeechInput,
            VoiceAction::Cancel => SettingsOp::CancelVoiceInstall,
        };
        feedback.send_failed = !send(op);
        if feedback.send_failed {
            return false;
        }
        self.pending.push_back(Pending {
            slot: Slot::Voice(intent.part, intent.action),
            failed: false,
            reset_draft: false,
        });
        true
    }

    pub fn speech_input_status(&self) -> Text {
        self.voice_status(SpeechInput)
    }

    pub fn voice_status(&self, part: VoicePart) -> Text {
        match (part, self.voice_part_state(part)) {
            (_, Some(PartState::NotInstalled)) => Text::VoiceNotInstalled,
            (_, Some(PartState::Ready)) => Text::VoiceReady,
            (_, Some(PartState::Unloaded)) => Text::VoiceUnloaded,
            (SpeechInput, None) => Text::VoiceStatusUnknown,
            (SpeechInput, Some(PartState::Verifying)) => Text::VoiceVerifying,
            (SpeechInput, Some(PartState::Downloading { .. })) => Text::VoiceDownloading,
            (SpeechInput, Some(PartState::Loading)) => Text::VoiceLoading,
            (
                SpeechInput,
                Some(
                    PartState::Unavailable
                    | PartState::Failed {
                        key: ErrorKey::VoiceUnavailable,
                        ..
                    },
                ),
            ) => Text::VoiceRestartRequired,
            (
                SpeechInput,
                Some(PartState::Failed {
                    installed: true, ..
                }),
            ) => Text::VoiceInstalledFailed,
            (
                SpeechInput,
                Some(PartState::Failed {
                    installed: false, ..
                }),
            ) => Text::VoiceNotInstalledFailed,
            (SpokenReplies, None) => Text::SpokenRepliesUnknown,
            (SpokenReplies, Some(PartState::Verifying)) => Text::SpokenRepliesVerifying,
            (SpokenReplies, Some(PartState::Downloading { .. })) => Text::SpokenRepliesDownloading,
            (SpokenReplies, Some(PartState::Loading)) => Text::SpokenRepliesLoading,
            (
                SpokenReplies,
                Some(
                    PartState::Unavailable
                    | PartState::Failed {
                        key: ErrorKey::VoiceUnavailable,
                        ..
                    },
                ),
            ) => Text::SpokenRepliesRestartRequired,
            (
                SpokenReplies,
                Some(PartState::Failed {
                    installed: true, ..
                }),
            ) => Text::SpokenRepliesInstalledFailed,
            (
                SpokenReplies,
                Some(PartState::Failed {
                    installed: false, ..
                }),
            ) => Text::SpokenRepliesNotInstalledFailed,
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

    pub fn stt_unload_index(&self) -> Option<usize> {
        let choice = self.stt_unload_choice()?;
        STT_UNLOAD_OPTIONS
            .iter()
            .position(|(policy, _)| *policy == choice)
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
