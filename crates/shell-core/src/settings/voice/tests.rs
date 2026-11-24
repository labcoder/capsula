use kumochi_protocol::message::{
    AssistantName, ErrorKey, MaxFrameRate, PartState, SettingsView, SttUnload, ThreadId, UserError,
    VoiceView,
};
use kumochi_protocol::voice::VoicePart::SpeechInput;

use super::super::{Settings, SettingsOp, Update};
use crate::strings::Text;

fn input_view(speech_input: PartState) -> VoiceView {
    VoiceView {
        speech_input,
        spoken_replies: PartState::Ready,
    }
}

fn saved(policy: SttUnload) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::default(),
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: policy,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
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
        assert!(state.receive_voice(Some(7), 7, input_view(input)));
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
fn talk_back_busy_leaves_the_saved_choice_and_only_its_answer_settles_a_retry() {
    use crate::link::output::Denied;

    let mut state = connected();
    assert!(!state.send_talk_back(true, || Err(Denied::Busy)));
    assert_eq!(state.talk_back_send_error(), Some(Text::TalkBackBusy));
    assert_eq!(state.talk_back_choice(), Some(false));
    assert!(!state.talk_back_pending());

    assert!(state.send_stt_unload(SttUnload::After15Min, |_| true));
    assert!(state.send_talk_back(true, || Ok(())));
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    assert_eq!(state.talk_back_choice(), Some(true));
    let mut settled = saved(SttUnload::After15Min);
    state.set_view(settled.clone(), Update::Answer);
    assert!(state.talk_back_pending());

    let error = UserError::from(ErrorKey::SettingsRefused);
    state.error(error.clone());
    state.set_view(settled.clone(), Update::Answer);
    assert_eq!(state.talk_back_choice(), Some(false));
    assert_eq!(state.talk_back_error(), Some(&error));
    assert!(state.send_talk_back(true, || Ok(())));
    settled.talk_back = true;
    state.set_view(settled, Update::Answer);
    assert_eq!(state.talk_back_choice(), Some(true));
    assert_eq!(state.talk_back_error(), None);
    assert_eq!(state.talk_back_send_error(), None);
    assert!(!state.talk_back_pending());
}

#[test]
fn the_load_off_refusal_clears_only_when_enable_is_saved() {
    for first_enable_fails in [false, true] {
        let mut state = connected();
        let mut off = saved(SttUnload::Kept);
        off.hold_to_talk = false;
        state.set_view(off.clone(), Update::Unsolicited);
        assert!(state.receive_voice(Some(7), 7, input_view(PartState::Unloaded)));
        let load = state.voice_setup_intent(SpeechInput).expect("load");
        assert!(state.send_voice_setup(load, Some(7), |op| op == SettingsOp::LoadSpeechInput));
        let refusal = UserError::from(ErrorKey::HoldToTalkOff);
        state.error(refusal.clone());
        state.set_view(off.clone(), Update::Answer);
        assert_eq!(state.voice_setup_error(SpeechInput), Some(&refusal));
        assert_eq!(state.notice(), Some(&refusal));

        assert!(state.send_hold_to_talk(true, |op| op == SettingsOp::SetHoldToTalk(true)));
        state.set_view(off.clone(), Update::Unsolicited);
        assert_eq!(state.voice_setup_error(SpeechInput), Some(&refusal));
        assert_eq!(state.notice(), Some(&refusal));

        let unrelated = UserError::from(ErrorKey::UsageLimit);
        let on = saved(SttUnload::Kept);
        if first_enable_fails {
            let refused_write = UserError::from(ErrorKey::SettingsRefused);
            state.error(refused_write.clone());
            state.set_view(off, Update::Answer);
            assert_eq!(state.voice_setup_error(SpeechInput), Some(&refusal));
            assert_eq!(state.hold_to_talk_error(), Some(&refused_write));
            state.error(unrelated.clone());
            assert!(state.send_hold_to_talk(true, |_| true));
            state.set_view(on.clone(), Update::Unsolicited);
            assert_eq!(state.voice_setup_error(SpeechInput), None);
            assert_eq!(state.notice(), Some(&unrelated));
            assert!(state.hold_to_talk_pending());
        }
        state.set_view(on, Update::Answer);
        assert_eq!(state.voice_setup_error(SpeechInput), None);
        assert_eq!(state.notice(), first_enable_fails.then_some(&unrelated));
        assert!(state.receive_voice(Some(7), 7, input_view(PartState::Ready)));
        assert_eq!(state.voice_setup_error(SpeechInput), None);
    }
}

#[test]
fn an_unavailable_owner_shows_restart_and_revokes_the_previous_setup_action() {
    let mut state = connected();
    assert!(state.receive_voice(Some(7), 7, input_view(PartState::Loading)));
    let pending = state
        .voice_setup_intent(SpeechInput)
        .expect("cancel while loading");
    assert!(state.receive_voice(Some(7), 7, input_view(PartState::Unavailable)));
    assert_eq!(state.speech_input_status(), Text::VoiceRestartRequired);
    assert_eq!(state.voice_setup_intent(SpeechInput), None);
    assert!(!state.send_voice_setup(pending, Some(7), |_| panic!(
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
        assert!(state.receive_voice(Some(7), 7, input_view(part)));
        assert_eq!(state.speech_input_status(), expected);
        assert!(!state.receive_voice(Some(7), 6, input_view(PartState::Ready)));
        assert!(!state.receive_voice(None, 7, input_view(PartState::Ready)));
        assert_eq!(state.speech_input_status(), expected);
    }
    state.disconnected();
    assert_eq!(state.speech_input_status(), Text::VoiceStatusUnknown);
}

#[test]
fn model_actions_keep_load_distinct_from_install_or_retry_and_never_invent_progress() {
    use super::VoiceAction::{Cancel, Install, Reload, Retry};

    for (part, action, op) in [
        (
            PartState::NotInstalled,
            Install,
            SettingsOp::InstallVoice(SpeechInput),
        ),
        (PartState::Unloaded, Reload, SettingsOp::LoadSpeechInput),
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
        assert!(state.receive_voice(Some(7), 7, input_view(part)));
        let intent = state.voice_setup_intent(SpeechInput).expect("eligible");
        assert_eq!(intent.action, action);
        assert_eq!(
            state
                .voice_setup_failure(SpeechInput)
                .map(|error| error.key()),
            match part {
                PartState::Failed { key, .. } => Some(key),
                _ => None,
            }
        );
        let mut sent = Vec::new();
        assert!(state.send_voice_setup(intent, Some(7), |message| {
            sent.push(message);
            true
        }));
        assert_eq!(sent, [op]);
        assert!(state.voice_setup_pending(SpeechInput));
        assert!(!state.send_voice_setup(intent, Some(7), |_| panic!("duplicate")));
        state.set_view(saved(SttUnload::Kept), Update::Answer);
        assert!(!state.voice_setup_pending(SpeechInput));
        assert_eq!(state.voice_part_state(SpeechInput), Some(part));
        assert!(!state.send_voice_setup(intent, Some(7), |_| false));
        assert!(state.voice_setup_send_failed(SpeechInput));
        assert!(!state.voice_setup_pending(SpeechInput));
        assert_eq!(state.voice_part_state(SpeechInput), Some(part));
    }
    let mut state = Settings::default();
    assert_eq!(state.voice_setup_intent(SpeechInput), None);
    assert!(state.receive_voice(Some(7), 7, input_view(PartState::NotInstalled)));
    assert_eq!(
        state.voice_setup_intent(SpeechInput),
        None,
        "no authoritative Settings"
    );
    state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
    assert!(state.receive_voice(Some(7), 7, input_view(PartState::Ready)));
    assert_eq!(state.voice_setup_intent(SpeechInput), None);
    state.disconnected();
    assert_eq!(state.voice_setup_intent(SpeechInput), None);
    assert_eq!(state.voice, None);
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
        assert!(state.receive_voice(Some(7), 7, input_view(before)));
        let old = state.voice_setup_intent(SpeechInput).expect("old intent");
        assert!(state.receive_voice(Some(7), 7, input_view(after)));
        assert!(!state.send_voice_setup(old, Some(7), |_| panic!("stale action")));
        assert_eq!(state.voice_part_state(SpeechInput), Some(after));
        assert!(!state.voice_setup_pending(SpeechInput));
    }
    for part in [PartState::NotInstalled, PartState::Loading] {
        let mut state = connected();
        assert!(state.receive_voice(Some(7), 7, input_view(part)));
        let old = state.voice_setup_intent(SpeechInput).expect("old intent");
        assert!(!state.send_voice_setup(old, Some(8), |_| panic!("new Session, old Voice")));
        assert!(!state.send_voice_setup(old, None, |_| panic!("missing Session")));
        assert!(state.voice_setup_send_failed(SpeechInput));
        state.disconnected();
        assert!(!state.voice_setup_send_failed(SpeechInput));
        assert_eq!(state.voice_part_state(SpeechInput), None);
        assert!(state.receive(Some(8), 8, saved(SttUnload::Kept), Update::Unsolicited));
        assert!(!state.receive_voice(Some(8), 7, input_view(part)));
        assert!(state.receive_voice(Some(8), 8, input_view(part)));
        assert!(!state.send_voice_setup(old, Some(8), |_| panic!("old same action")));
        assert!(!state.send_voice_setup(old, Some(7), |_| panic!("old Session")));
        let fresh = state.voice_setup_intent(SpeechInput).expect("fresh intent");
        assert_ne!(old, fresh);
        assert!(state.send_voice_setup(fresh, Some(8), |_| true));
    }
    let mut state = connected();
    assert!(state.receive_voice(Some(7), 7, input_view(PartState::Verifying)));
    let cancel = state.voice_setup_intent(SpeechInput).expect("cancel");
    assert!(state.receive_voice(
        Some(7),
        7,
        input_view(PartState::Downloading { done: 2, total: 3 })
    ));
    assert_eq!(state.voice_setup_intent(SpeechInput), Some(cancel));
    assert!(state.send_voice_setup(cancel, Some(7), |_| true));
}

#[test]
fn model_refusals_follow_the_shared_fifo_and_survive_unrelated_answers() {
    for model_first in [true, false] {
        let mut state = connected();
        assert!(state.receive_voice(Some(7), 7, input_view(PartState::Unloaded)));
        let intent = state.voice_setup_intent(SpeechInput).expect("load");
        if model_first {
            assert!(state.send_voice_setup(intent, Some(7), |_| true));
        }
        assert!(state.send_stt_unload(SttUnload::After15Min, |_| true));
        if !model_first {
            assert!(state.send_voice_setup(intent, Some(7), |_| true));
        }
        assert!(state.voice_setup_pending(SpeechInput));
        assert!(state.stt_unload_pending());
        assert_eq!(state.error(ErrorKey::VoiceUnavailable.into()), None);
        assert_eq!(
            state.voice_setup_error(SpeechInput).map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Unsolicited));
        assert!(!state.receive(Some(7), 6, saved(SttUnload::Kept), Update::Answer));
        assert!(state.voice_setup_pending(SpeechInput));
        assert!(state.stt_unload_pending());
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Answer));
        assert_eq!(state.voice_setup_pending(SpeechInput), !model_first);
        assert_eq!(state.stt_unload_pending(), model_first);
        assert!(state.receive(Some(7), 7, saved(SttUnload::Kept), Update::Answer));
        assert!(!state.voice_setup_pending(SpeechInput));
        assert!(!state.stt_unload_pending());
        assert_eq!(
            state.voice_part_state(SpeechInput),
            Some(PartState::Unloaded)
        );
        assert_eq!(
            state.voice_setup_error(SpeechInput).map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
        assert!(state.send_voice_setup(intent, Some(7), |_| true));
        state.set_view(saved(SttUnload::Kept), Update::Unsolicited);
        assert_eq!(
            state.voice_setup_error(SpeechInput).map(UserError::key),
            model_first.then_some(ErrorKey::VoiceUnavailable)
        );
        state.set_view(saved(SttUnload::Kept), Update::Answer);
        assert_eq!(state.voice_setup_error(SpeechInput), None);
        assert_eq!(
            state.stt_unload_error().map(UserError::key),
            (!model_first).then_some(ErrorKey::VoiceUnavailable)
        );
    }
}

#[test]
fn spoken_replies_setup_tracks_its_own_status_and_needs_an_explicit_request() {
    use super::VoiceAction::{Cancel, Install, Retry};
    use crate::link::{LinkHandle, Session};
    use crate::settings::Save;
    use kumochi_protocol::message::{ControlToBrain, Incarnation};
    use kumochi_protocol::voice::VoicePart::SpokenReplies;

    let mut state = connected();
    let mut link = LinkHandle::detached();
    let controls = link.record_controls();
    let session = Session::detached(7, Incarnation(1));
    let pair = Some((&link, &session));
    let view = |spoken_replies| VoiceView {
        speech_input: PartState::Ready,
        spoken_replies,
    };
    assert!(state.receive_voice(Some(7), 7, view(PartState::NotInstalled)));
    assert_eq!(
        state.apply_save(Save::TalkBack(true), pair).result,
        Ok(true)
    );
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::SetTalkBack(true)))
    );
    let mut answer = saved(SttUnload::Kept);
    answer.talk_back = true;
    state.set_view(answer.clone(), Update::Answer);
    assert!(
        controls.is_empty(),
        "enabling the preference must not install a model"
    );

    for (status, text, action, op) in [
        (
            PartState::NotInstalled,
            Text::VoiceNotInstalled,
            Install,
            SettingsOp::InstallVoice(SpokenReplies),
        ),
        (
            PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: true,
            },
            Text::SpokenRepliesInstalledFailed,
            Retry,
            SettingsOp::InstallVoice(SpokenReplies),
        ),
        (
            PartState::Downloading { done: 1, total: 2 },
            Text::SpokenRepliesDownloading,
            Cancel,
            SettingsOp::CancelVoiceInstall,
        ),
    ] {
        assert!(state.receive_voice(Some(7), 7, view(status)));
        assert_eq!(state.speech_input_status(), Text::VoiceReady);
        assert_eq!(state.voice_setup_intent(SpeechInput), None);
        assert_eq!(state.voice_status(SpokenReplies), text);
        let intent = state
            .voice_setup_intent(SpokenReplies)
            .expect("spoken replies action");
        assert_eq!(intent.action, action);
        assert_eq!(state.apply_save(Save::Voice(intent), pair).result, Ok(true));
        assert_eq!(controls.try_recv(), Ok(ControlToBrain::Settings(op)));
        state.set_view(answer.clone(), Update::Answer);
        assert_eq!(
            state.voice_status(SpokenReplies),
            text,
            "acks do not complete setup"
        );
    }
    assert!(state.receive_voice(Some(7), 7, view(PartState::Ready)));
    assert_eq!(state.voice_status(SpokenReplies), Text::VoiceReady);
    assert_eq!(state.voice_setup_intent(SpokenReplies), None);
    assert!(state.receive_voice(
        Some(7),
        7,
        view(PartState::Failed {
            key: ErrorKey::VoiceUnavailable,
            installed: true,
        })
    ));
    assert_eq!(
        state.voice_status(SpokenReplies),
        Text::SpokenRepliesRestartRequired
    );
    assert_eq!(state.voice_setup_intent(SpokenReplies), None);
}

#[test]
fn only_current_queued_cancel_that_affects_input_requests_local_input_revocation() {
    use crate::link::{LinkHandle, Session};
    use crate::settings::{AfterSave, Save};
    use kumochi_protocol::message::{ControlToBrain, Incarnation};
    use kumochi_protocol::voice::VoicePart::SpokenReplies;

    let mut link = LinkHandle::detached();
    let controls = link.record_controls();
    let old_session = Session::detached(6, Incarnation(1));
    let session = Session::detached(7, Incarnation(1));
    for (input, current, expected) in [
        (PartState::Ready, &session, None),
        (PartState::Loading, &old_session, None),
        (PartState::Loading, &session, Some(AfterSave::CancelVoice)),
    ] {
        let mut state = connected();
        assert!(state.receive_voice(
            Some(7),
            7,
            VoiceView {
                speech_input: input,
                spoken_replies: PartState::Downloading { done: 1, total: 2 },
            }
        ));
        let save = Save::Voice(state.voice_setup_intent(SpokenReplies).expect("cancel"));
        assert_eq!(save.before(), None);
        let result = state.apply_save(save, Some((&link, current)));
        let queued = current.generation() == 7;
        assert_eq!(result.result, Ok(queued));
        assert_eq!(result.after, expected);
        if queued {
            assert_eq!(
                controls.try_recv(),
                Ok(ControlToBrain::Settings(SettingsOp::CancelVoiceInstall))
            );
        } else {
            assert!(controls.is_empty(), "stale callback must not send");
        }
    }
}

#[test]
fn show_screen_waits_for_its_own_answer_and_a_refusal_restores_the_saved_choice() {
    let mut saved = saved(SttUnload::Kept);
    saved.screen_holds = false;
    saved.show_screen = true;
    let mut state = Settings::default();
    assert_eq!(state.show_screen_choice(), None);
    assert!(!state.send_show_screen(false, |_| panic!("nothing saved to change")));
    state.set_view(saved.clone(), Update::Unsolicited);
    assert_eq!(state.show_screen_choice(), Some(true));
    assert!(!state.send_show_screen(true, |_| panic!("already on")));
    assert!(!state.send_show_screen(false, |_| false));
    assert!(!state.show_screen_pending());
    assert_eq!(state.show_screen_choice(), Some(true));

    assert!(state.send_show_screen(false, |op| op == SettingsOp::SetShowScreen(false)));
    assert!(state.show_screen_pending());
    assert_eq!(state.show_screen_choice(), Some(false));
    assert!(!state.send_show_screen(true, |_| panic!("one save remains in flight")));
    state.error(ErrorKey::SettingsRefused.into());
    state.set_view(saved.clone(), Update::Answer);
    assert!(!state.show_screen_pending());
    assert_eq!(state.show_screen_choice(), Some(true));
    assert_eq!(
        state.show_screen_error().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert_eq!(state.hold_to_talk_error(), None);

    assert!(state.send_show_screen(false, |_| true));
    saved.show_screen = false;
    state.set_view(saved, Update::Answer);
    assert!(!state.show_screen_pending());
    assert_eq!(state.show_screen_choice(), Some(false));
    assert_eq!(state.show_screen_error(), None);
    assert_eq!(state.hold_to_talk_choice(), Some(true));
}
