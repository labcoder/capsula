use kumochi_protocol::message::{
    AssistantName, ErrorKey, MaxFrameRate, PartState, SettingsView, SttUnload, ThreadId, UserError,
};

use super::super::{Settings, SettingsOp, Update};
use crate::strings::Text;

fn saved(policy: SttUnload) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::default(),
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: policy,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
    }
}

fn connected() -> Settings {
    let mut state = Settings::default();
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    state
}

#[test]
fn ready_loading_and_unloaded_input_permit_a_deliberate_hold() {
    let mut state = connected();
    for input in [PartState::Ready, PartState::Loading, PartState::Unloaded] {
        assert!(state.receive_speech_input(Some(7), 7, input));
        assert_eq!(state.hold_to_talk_notice(), None);
    }
    state.disconnected();
    assert_eq!(state.hold_to_talk_notice(), Some(Text::VoiceStatusUnknown));
}

#[test]
fn hold_to_talk_waits_for_its_own_answer_and_refusal_restores_the_saved_choice() {
    let mut state = connected();
    assert!(state.hold_to_talk_enabled());
    assert!(state.send_hold_to_talk(false, |op| op == SettingsOp::SetHoldToTalk(false)));
    assert!(!state.hold_to_talk_enabled());
    assert!(state.hold_to_talk_pending());
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    assert!(!state.hold_to_talk_enabled());
    assert!(!state.send_hold_to_talk(true, |_| panic!("one save remains in flight")));
    state.error(ErrorKey::SettingsRefused.into());
    state.set_view(saved(SttUnload::Kept), Update::Answer);
    assert!(!state.hold_to_talk_pending());
    assert!(state.hold_to_talk_enabled());
    assert_eq!(
        state.hold_to_talk_error().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert!(state.send_hold_to_talk(false, |_| true));
    let mut settled = saved(SttUnload::Kept);
    settled.hold_to_talk = false;
    state.set_view(settled.clone(), Update::Answer);
    assert!(!state.hold_to_talk_enabled());
    assert_eq!(state.hold_to_talk_error(), None);
    assert!(!state.send_hold_to_talk(true, |_| false));
    assert_eq!(state.hold_to_talk_choice(), Some(false));
    assert!(!state.hold_to_talk_pending());
    assert!(state.send_hold_to_talk(true, |_| true));
    assert!(!state.hold_to_talk_enabled());
    settled.hold_to_talk = true;
    state.set_view(settled, Update::Answer);
    assert!(state.hold_to_talk_enabled());
}

#[test]
fn an_unavailable_owner_shows_restart_and_revokes_the_previous_setup_action() {
    let mut state = connected();
    assert!(state.receive_speech_input(Some(7), 7, PartState::Loading));
    let pending = state.speech_input_intent().expect("cancel while loading");
    assert!(state.receive_speech_input(Some(7), 7, PartState::Unavailable));
    assert_eq!(state.speech_input_status(), Text::VoiceRestartRequired);
    assert_eq!(state.speech_input_intent(), None);
    assert!(!state.send_speech_input(pending, Some(7), |_| panic!(
        "dead owner must not receive setup"
    )));
    assert_eq!(
        crate::strings::Strings::english().get(state.speech_input_status()),
        "Speech input unavailable. Restart the app."
    );
}

#[test]
fn idle_choice_waits_for_its_fifo_answer_and_does_not_complete_another_setting() {
    let mut state = connected();
    assert!(state.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true));
    assert!(state.send_stt_unload(SttUnload::After15Min, |op| {
        assert_eq!(op, SettingsOp::SetSttUnload(SttUnload::After15Min));
        true
    }));
    assert_eq!(state.view().expect("saved").stt_unload, SttUnload::Kept);
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::After15Min));
    assert!(state.stt_unload_pending());
    assert!(!state.send_stt_unload(SttUnload::After1Hour, |_| panic!(
        "pending choice must not send"
    )));
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    assert!(state.stt_unload_pending());
    state.set_view(saved(SttUnload::Kept), Update::Answer);
    assert!(state.stt_unload_pending());
    state.set_view(saved(SttUnload::After15Min), Update::Answer);
    assert!(!state.stt_unload_pending());
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::After15Min));
}

#[test]
fn a_refused_idle_write_restores_saved_policy_until_its_successful_retry() {
    let mut state = connected();
    assert!(state.send_stt_unload(SttUnload::After1Hour, |_| true));
    let refused = ErrorKey::SettingsRefused.into();
    assert_eq!(state.error(refused), None);
    assert!(state.stt_unload_pending());
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
    assert_eq!(
        state.stt_unload_error().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    state.set_view(saved(SttUnload::Kept), Update::Answer);
    assert!(!state.stt_unload_pending());
    assert!(state.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true));
    state.set_view(saved(SttUnload::Kept), Update::Answer);
    assert_eq!(
        state.stt_unload_error().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert!(state.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true));
    state.error(ErrorKey::ConfigUnreadable.into());
    state.set_view(saved(SttUnload::Kept), Update::Answer);
    assert_eq!(
        state.stt_unload_error().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert!(state.send_stt_unload(SttUnload::After1Hour, |_| true));
    state.set_view(saved(SttUnload::After1Hour), Update::Unsolicited);
    assert!(state.stt_unload_pending());
    assert!(state.stt_unload_error().is_some());
    state.set_view(saved(SttUnload::After1Hour), Update::Answer);
    assert!(!state.stt_unload_pending());
    assert_eq!(state.stt_unload_error(), None);
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::After1Hour));
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::ConfigUnreadable)
    );
}

#[test]
fn transport_failure_and_disconnect_never_claim_a_committed_idle_choice() {
    let mut state = connected();
    assert!(!state.send_stt_unload(SttUnload::Kept, |_| panic!("same value")));
    assert!(!state.send_stt_unload(SttUnload::After15Min, |_| false));
    assert!(state.stt_unload_send_failed());
    assert!(!state.stt_unload_pending());
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
    assert!(state.send_stt_unload(SttUnload::After15Min, |_| true));
    assert!(!state.stt_unload_send_failed());
    state.disconnected();
    assert!(!state.stt_unload_pending());
    assert_eq!(state.stt_unload_choice(), None);
    assert!(!state.send_stt_unload(SttUnload::After1Hour, |_| panic!("offline")));
    assert!(!state.receive(Some(8), 7, saved(SttUnload::After15Min), Update::Answer));
    assert_eq!(state.stt_unload_choice(), None);
    assert!(state.receive(Some(8), 8, saved(SttUnload::Kept), Update::Unsolicited));
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
}

#[test]
fn speech_status_requires_the_current_session_and_does_not_survive_disconnect() {
    let mut state = connected();
    assert_eq!(state.speech_input_status(), Text::VoiceStatusUnknown);
    for (part, expected) in [
        (PartState::NotInstalled, Text::VoiceNotInstalled),
        (PartState::Verifying, Text::VoiceVerifying),
        (
            PartState::Downloading { done: 1, total: 2 },
            Text::VoiceDownloading,
        ),
        (PartState::Loading, Text::VoiceLoading),
        (PartState::Ready, Text::VoiceReady),
        (PartState::Unloaded, Text::VoiceUnloaded),
        (
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: true,
            },
            Text::VoiceInstalledFailed,
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: false,
            },
            Text::VoiceNotInstalledFailed,
        ),
    ] {
        assert!(state.receive_speech_input(Some(7), 7, part));
        assert_eq!(state.speech_input_status(), expected);
        assert!(!state.receive_speech_input(Some(7), 6, PartState::Ready));
        assert!(!state.receive_speech_input(None, 7, PartState::Ready));
        assert_eq!(state.speech_input_status(), expected);
    }
    state.disconnected();
    assert_eq!(state.speech_input_status(), Text::VoiceStatusUnknown);
}

#[test]
fn model_actions_send_only_the_existing_operation_and_never_invent_progress() {
    use super::SpeechInputAction::{Cancel, Install, Reload, Retry};
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    for (part, action, op) in [
        (
            PartState::NotInstalled,
            Install,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (
            PartState::Unloaded,
            Reload,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: true,
            },
            Retry,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceDownload,
                installed: false,
            },
            Retry,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceModelPath,
                installed: true,
            },
            Retry,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceModelPath,
                installed: false,
            },
            Retry,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (PartState::Verifying, Cancel, SettingsOp::CancelVoiceInstall),
        (
            PartState::Downloading { done: 1, total: 2 },
            Cancel,
            SettingsOp::CancelVoiceInstall,
        ),
        (PartState::Loading, Cancel, SettingsOp::CancelVoiceInstall),
    ] {
        let mut state = connected();
        assert!(state.receive_speech_input(Some(7), 7, part));
        let intent = state.speech_input_intent().expect("eligible");
        assert_eq!(intent.action, action);
        assert_eq!(
            state.speech_input_failure().map(|error| error.key()),
            match part {
                PartState::Failed { key, .. } => Some(key),
                _ => None,
            }
        );
        let mut sent = Vec::new();
        assert!(state.send_speech_input(intent, Some(7), |message| {
            sent.push(message);
            true
        }));
        assert_eq!(sent, [op]);
        assert!(state.speech_input_pending());
        assert!(!state.send_speech_input(intent, Some(7), |_| panic!("duplicate")));
        state.set_view(saved(SttUnload::Kept), Update::Answer);
        assert!(!state.speech_input_pending());
        assert_eq!(state.speech_input, Some(part));
        assert!(!state.send_speech_input(intent, Some(7), |_| false));
        assert!(state.speech_input_send_failed());
        assert!(!state.speech_input_pending());
        assert_eq!(state.speech_input, Some(part));
    }
    let mut state = Settings::default();
    assert_eq!(state.speech_input_intent(), None);
    assert!(state.receive_speech_input(Some(7), 7, PartState::NotInstalled));
    assert_eq!(
        state.speech_input_intent(),
        None,
        "no authoritative Settings"
    );
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    assert!(state.receive_speech_input(Some(7), 7, PartState::Ready));
    assert_eq!(state.speech_input_intent(), None);
    state.disconnected();
    assert_eq!(state.speech_input_intent(), None);
    assert_eq!(state.speech_generation, None);
}

#[test]
fn captured_model_intents_cannot_change_meaning_or_cross_connections() {
    for (before, after) in [
        (
            PartState::Unloaded,
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: true,
            },
        ),
        (PartState::Loading, PartState::Ready),
        (PartState::NotInstalled, PartState::Ready),
    ] {
        let mut state = connected();
        assert!(state.receive_speech_input(Some(7), 7, before));
        let old = state.speech_input_intent().expect("old intent");
        assert!(state.receive_speech_input(Some(7), 7, after));
        assert!(!state.send_speech_input(old, Some(7), |_| panic!("stale action")));
        assert_eq!(state.speech_input, Some(after));
        assert!(!state.speech_input_pending());
    }
    for part in [PartState::NotInstalled, PartState::Loading] {
        let mut state = connected();
        assert!(state.receive_speech_input(Some(7), 7, part));
        let old = state.speech_input_intent().expect("old intent");
        assert!(!state.send_speech_input(old, Some(8), |_| panic!("new Session, old Voice")));
        assert!(!state.send_speech_input(old, None, |_| panic!("missing Session")));
        assert!(state.speech_input_send_failed());
        state.disconnected();
        assert!(!state.speech_input_send_failed());
        assert_eq!(state.speech_input, None);
        assert!(state.receive(Some(8), 8, saved(SttUnload::Kept), Update::Unsolicited));
        assert!(!state.receive_speech_input(Some(8), 7, part));
        assert!(state.receive_speech_input(Some(8), 8, part));
        assert!(!state.send_speech_input(old, Some(8), |_| panic!("old same action")));
        assert!(!state.send_speech_input(old, Some(7), |_| panic!("old Session")));
        let fresh = state.speech_input_intent().expect("fresh intent");
        assert_ne!(old, fresh);
        assert!(state.send_speech_input(fresh, Some(8), |_| true));
    }
    let mut state = connected();
    assert!(state.receive_speech_input(Some(7), 7, PartState::Verifying));
    let cancel = state.speech_input_intent().expect("cancel");
    assert!(state.receive_speech_input(Some(7), 7, PartState::Downloading { done: 2, total: 3 }));
    assert_eq!(state.speech_input_intent(), Some(cancel));
    assert!(state.send_speech_input(cancel, Some(7), |_| true));
}

#[test]
fn model_refusals_follow_the_shared_fifo_and_survive_unrelated_answers() {
    for model_first in [true, false] {
        let mut state = connected();
        assert!(state.receive_speech_input(Some(7), 7, PartState::Unloaded));
        let intent = state.speech_input_intent().expect("load");
        if model_first {
            assert!(state.send_speech_input(intent, Some(7), |_| true));
        }
        assert!(state.send_stt_unload(SttUnload::After15Min, |_| true));
        if !model_first {
            assert!(state.send_speech_input(intent, Some(7), |_| true));
        }
        assert!(state.speech_input_pending());
        assert!(state.stt_unload_pending());
        assert_eq!(state.error(ErrorKey::VoiceUnavailable.into()), None);
        assert_eq!(
            state.speech_input_error().map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Unsolicited));
        assert!(!state.receive(Some(7), 6, saved(SttUnload::Kept), Update::Answer));
        assert!(state.speech_input_pending());
        assert!(state.stt_unload_pending());
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Answer));
        assert_eq!(state.speech_input_pending(), !model_first);
        assert_eq!(state.stt_unload_pending(), model_first);
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Answer));
        assert!(!state.speech_input_pending());
        assert!(!state.stt_unload_pending());
        assert_eq!(state.speech_input, Some(PartState::Unloaded));
        assert_eq!(
            state.speech_input_error().map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
        assert!(state.send_speech_input(intent, Some(7), |_| true));
        state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
        assert_eq!(
            state.speech_input_error().map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        state.set_view(saved(SttUnload::Kept), Update::Answer);
        assert_eq!(state.speech_input_error(), None);
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
    }
}
