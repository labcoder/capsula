use super::{Completion, Error, Field, Settings, Update};
use crate::strings::Text;
use kumochi_protocol::message::{AssistantName, SettingsView, ThreadId, UserName};

fn view(assistant: &str, user: Option<&str>) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid"),
        user_name: user.map(|name| UserName::new(name).expect("valid")),
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: kumochi_protocol::message::DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
    }
}

fn connected() -> Settings {
    let mut settings = Settings::default();
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
        None
    );
    settings
}

#[test]
fn followup_cancel_during_save_cannot_resave_the_old_name_on_blur() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Cloud");
    assert!(settings.save(Field::Assistant, |_| true));
    settings.cancel(Field::Assistant);
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert!(!settings.editing(Field::Assistant));
    assert!(!settings.save(Field::Assistant, |_| panic!("blur reverted the name")));
}

#[test]
fn followup_current_view_clears_offline_but_not_validation_errors() {
    let mut settings = connected();
    settings.disconnected();
    settings.change(Field::Assistant, "Cloud");
    assert!(!settings.save(Field::Assistant, |_| panic!("offline send")));
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    settings.change(Field::User, &"x".repeat(25));
    assert!(!settings.save(Field::User, |_| panic!("invalid send")));
    assert!(!settings.receive(Some(2), 1, view("Kumo", Some("Oscar")), Update::Unsolicited));
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    assert!(settings.receive(Some(2), 2, view("Kumo", Some("Oscar")), Update::Unsolicited));
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Validation(super::NameError::TooLong))
    );
}

#[test]
fn followup_inline_rename_refusals_stay_out_of_settings_fields() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Cloud");
    assert!(settings.save(Field::Assistant, |_| true));
    assert!(
        !settings.send_assistant(AssistantName::new("Later").expect("valid"), |_| panic!(
            "busy send"
        ))
    );
    assert_eq!(settings.rename_refusal(), Text::RenameBusy);
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    settings.disconnected();
    assert!(
        !settings.send_assistant(AssistantName::new("Later").expect("valid"), |_| panic!(
            "offline send"
        ))
    );
    assert_eq!(settings.rename_refusal(), Text::TurnOffline);
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.dock_notice(), None);
    assert!(settings.receive(Some(2), 2, view("Kumo", Some("Oscar")), Update::Unsolicited));
    assert!(!settings.send_assistant(AssistantName::new("Later").expect("valid"), |_| false));
    assert_eq!(settings.rename_refusal(), Text::SettingsUnavailable);
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.dock_notice(), None);
}

#[test]
fn followup_invalid_dismissed_edits_keep_a_dock_notice_until_corrected() {
    let mut settings = connected();
    settings.change(Field::Assistant, "");
    assert!(!settings.save(Field::Assistant, |_| panic!("invalid dismissal save")));
    assert_eq!(settings.dock_notice(), Some(Text::SettingsNotSaved));
    assert!(!settings.save(Field::Assistant, |_| panic!("invalid post-dismissal blur")));
    assert_eq!(settings.dock_notice(), Some(Text::SettingsNotSaved));
    assert_eq!(settings.value(Field::Assistant), "");
    assert_eq!(settings.view(), Some(&view("Kumo", Some("Oscar"))));
    settings.disconnected();
    assert_eq!(settings.dock_notice(), Some(Text::SettingsNotSaved));
    assert!(settings.receive(Some(2), 2, view("Kumo", Some("Oscar")), Update::Unsolicited));
    assert_eq!(settings.dock_notice(), Some(Text::SettingsNotSaved));
    settings.cancel(Field::Assistant);
    assert_eq!(settings.dock_notice(), None);
    assert_eq!(settings.value(Field::Assistant), "Kumo");
}

#[test]
fn followup_settings_error_keys_mark_only_the_fifo_head() {
    use kumochi_protocol::message::ErrorKey;
    for key in [
        ErrorKey::SettingsRefused,
        ErrorKey::ConfigUnreadable,
        ErrorKey::VoiceUnavailable,
    ] {
        let mut settings = connected();
        settings.change(Field::User, "Alex");
        assert!(settings.save(Field::User, |_| true));
        assert!(settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true));
        assert_eq!(settings.error(key.into()), Some(Field::User));
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(key.into()))
        );
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert_eq!(settings.dock_notice(), Some(Text::DockError));
        assert_eq!(
            settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: true,
                field: Field::User
            })
        );
        assert_eq!(
            settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.notice(), Some(&key.into()));
    }
}
