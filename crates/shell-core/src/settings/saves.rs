//! Settings save dispatch and the native work that surrounds a save.

use kumochi_protocol::message::{
    AssistantName, ConnectionKind, ControlToBrain, DockPosition, MaxFrameRate, ModelSlug, SttUnload,
};
use kumochi_protocol::voice::VoicePart;

use super::{
    Field, Settings, SettingsOp, SettingsView, TaskbarChoice, Update, VoiceAction, VoiceIntent,
};
use crate::link::{LinkHandle, Session, output::Denied};
use crate::strings::Text;

/// The host's save request, before transport and reply handling.
pub enum Save {
    Name(Field),
    RenameAssistant(AssistantName),
    DefaultModel(ConnectionKind, ModelSlug),
    FrameRate(MaxFrameRate),
    DockPosition(DockPosition),
    Taskbar(TaskbarChoice),
    HoldToTalk(bool),
    TalkBack(bool),
    ShowScreen(bool),
    SttUnload(SttUnload),
    Voice(VoiceIntent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeforeSave {
    RevokeScreenOffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterSave {
    CancelVoice,
    FollowAndCancelVoice,
    RefreshScreenOffer { clear_marks: bool },
    SyncTaskbar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveOutcome {
    /// Whether the setting was queued, or the immediate refusal the caller should show.
    pub result: Result<bool, Text>,
    pub notify: bool,
    pub after: Option<AfterSave>,
}

impl Save {
    /// Work the host does before offering the save to the current session.
    pub fn before(&self) -> Option<BeforeSave> {
        match self {
            Self::ShowScreen(false) => Some(BeforeSave::RevokeScreenOffer),
            _ => None,
        }
    }
}

type Pair<'a> = Option<(&'a LinkHandle, &'a Session)>;

fn send(pair: Pair<'_>, op: SettingsOp) -> bool {
    pair.is_some_and(|(link, session)| link.send_control(session, ControlToBrain::Settings(op)))
}

impl Settings {
    pub fn apply_save(&mut self, save: Save, pair: Pair<'_>) -> SaveOutcome {
        use AfterSave::{CancelVoice, FollowAndCancelVoice, RefreshScreenOffer, SyncTaskbar};

        let send = |op| send(pair, op);
        let rename = matches!(save, Save::RenameAssistant(_));
        let always_notify = !matches!(save, Save::DefaultModel(..) | Save::FrameRate(_));
        let mut after = None;
        let queued = match save {
            Save::Name(field) => self.save(field, send),
            Save::RenameAssistant(name) => self.send_assistant(name, send),
            Save::DefaultModel(connection, model) => {
                self.send_default_model(connection, model, send)
            }
            Save::FrameRate(rate) => self.send_max_frame_rate(rate, send),
            Save::DockPosition(position) => self.send_dock_position(position, send),
            Save::Taskbar(choice) => {
                let result = self.send_taskbar(choice, send);
                let queued = result == Ok(true);
                return SaveOutcome {
                    result,
                    notify: queued,
                    after: queued.then_some(SyncTaskbar),
                };
            }
            Save::HoldToTalk(on) => {
                let queued = self.send_hold_to_talk(on, send);
                after = (queued && !on).then_some(FollowAndCancelVoice);
                queued
            }
            Save::TalkBack(on) => self.send_talk_back(on, || {
                pair.map_or(Err(Denied::Stale), |(link, session)| {
                    link.send_talk_back(session, on)
                })
            }),
            Save::ShowScreen(on) => {
                after = Some(RefreshScreenOffer { clear_marks: !on });
                self.send_show_screen(on, send)
            }
            Save::SttUnload(policy) => self.send_stt_unload(policy, send),
            Save::Voice(intent) => {
                let queued = self.send_voice_setup(
                    intent,
                    pair.map(|(_, session)| session.generation()),
                    send,
                );
                let changes_input = if intent.action == VoiceAction::Cancel {
                    self.voice_setup_intent(VoicePart::SpeechInput)
                        .is_some_and(|input| input.action == VoiceAction::Cancel)
                } else {
                    intent.part == VoicePart::SpeechInput
                };
                after = (queued && changes_input).then_some(CancelVoice);
                queued
            }
        };
        SaveOutcome {
            result: if rename && !queued {
                Err(self.rename_refusal())
            } else {
                Ok(queued)
            },
            notify: always_notify || queued,
            after,
        }
    }

    /// A current settings view can repay an offline dock move before the host redraws it.
    pub fn receive_and_replay_dock(
        &mut self,
        pair: Pair<'_>,
        generation: u64,
        view: SettingsView,
        update: Update,
    ) -> bool {
        if !self.receive(
            pair.map(|(_, session)| session.generation()),
            generation,
            view,
            update,
        ) {
            return false;
        }
        self.flush_offline_position(|op| send(pair, op));
        true
    }
}
