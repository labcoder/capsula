use kumochi_protocol::message::{ErrorKey, ThreadId, UserError};

use super::{AssistantName, Error, Field, Settings, SettingsView, Update, UserName};

fn view(assistant: &str, user: &str) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
        user_name: Some(UserName::new(user).expect("valid user name")),
        onboarded: true,
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

#[test]
fn dock_notice_survives_unrelated_success_and_reconnect_until_matching_retry() {
    let mut settings = Settings::default();
    assert!(
        settings.receive(Some(1), 1, view("Kumo", "Oscar"), Update::Unsolicited),
        "the first connection supplies settings"
    );
    settings.change(Field::User, "Alex");
    assert!(
        settings.save(Field::User, |_| true),
        "the user save is first"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the inline assistant rename is second"
    );
    let error = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(settings.error(error.clone()), Some(Field::User));
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(1), 1, view("Kumo", "Oscar"), Update::Answer),
        "the first save is refused"
    );
    assert_eq!(settings.value(Field::User), "Oscar");
    assert!(
        !settings.field(Field::User).saving,
        "the refused save settled"
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "the rename is still pending"
    );
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(error.clone()))
    );
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(1), 1, view("Cloud", "Oscar"), Update::Answer),
        "the unrelated assistant rename succeeds"
    );
    assert_eq!(settings.view(), Some(&view("Cloud", "Oscar")));
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert!(
        !settings.field(Field::Assistant).saving,
        "the rename settled"
    );
    assert_eq!(settings.notice(), Some(&error));
    settings.disconnected();
    assert_eq!(settings.view(), None);
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(2), 2, view("Cloud", "Oscar"), Update::Unsolicited),
        "the next connection restores settings"
    );
    assert_eq!(settings.notice(), Some(&error));
    settings.change(Field::User, "Alex");
    assert_eq!(settings.field(Field::User).error, None);
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.save(Field::User, |_| true),
        "the matching field retries"
    );
    assert!(
        settings.receive(Some(2), 2, view("Cloud", "Oscar"), Update::Unsolicited),
        "an unsolicited view is not the retry's answer"
    );
    assert!(
        settings.field(Field::User).saving,
        "the retry remains pending"
    );
    assert_eq!(settings.value(Field::User), "Alex");
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(2), 2, view("Cloud", "Alex"), Update::Answer),
        "the matching retry succeeds"
    );
    assert_eq!(settings.view(), Some(&view("Cloud", "Alex")));
    assert_eq!(settings.value(Field::User), "Alex");
    assert!(!settings.field(Field::User).saving, "the retry settled");
    assert_eq!(settings.field(Field::User).error, None);
    assert_eq!(settings.notice(), None);
}

#[test]
fn unattributed_notice_survives_success_until_explicitly_cleared() {
    let mut settings = Settings::default();
    let error = UserError::from(ErrorKey::ConfigUnreadable);
    assert_eq!(settings.error(error.clone()), None);
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(1), 1, view("Kumo", "Oscar"), Update::Unsolicited),
        "the initial settings do not dismiss an unattributed error"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the assistant rename is sent"
    );
    assert!(
        settings.receive(Some(1), 1, view("Cloud", "Oscar"), Update::Answer),
        "the assistant rename succeeds"
    );
    assert_eq!(settings.notice(), Some(&error));
    settings.clear_notice();
    assert_eq!(settings.notice(), None);
    assert_eq!(settings.view(), Some(&view("Cloud", "Oscar")));
    assert_eq!(settings.value(Field::Assistant), "Cloud");
}

#[test]
fn receive_fences_stale_and_missing_generations_before_any_state_mutation() {
    let mut settings = Settings::default();
    assert!(
        settings.receive(Some(7), 7, view("Kumo", "Oscar"), Update::Unsolicited),
        "the current generation supplies settings"
    );
    settings.change(Field::User, "Alex");
    assert!(
        settings.save(Field::User, |_| true),
        "the user save is first"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the assistant rename is second"
    );
    let error = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(settings.error(error.clone()), Some(Field::User));
    settings.change(Field::Assistant, "Later");
    for (current, generation) in [(Some(7), 6), (None, 7)] {
        assert!(
            !settings.receive(
                current,
                generation,
                view("Stale", "Mallory"),
                Update::Answer
            ),
            "a stale or missing generation cannot apply a settings answer"
        );
        assert_eq!(settings.view(), Some(&view("Kumo", "Oscar")));
        assert_eq!(settings.value(Field::Assistant), "Later");
        assert_eq!(settings.value(Field::User), "Alex");
        assert!(
            settings.field(Field::Assistant).saving,
            "the rename remains pending"
        );
        assert!(
            settings.field(Field::User).saving,
            "the user save remains pending"
        );
        assert_eq!(
            settings.field(Field::User).error,
            Some(Error::Brain(error.clone()))
        );
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert_eq!(settings.notice(), Some(&error));
    }
    assert!(
        settings.receive(Some(7), 7, view("Remote", "Blake"), Update::Unsolicited),
        "the current generation's unsolicited view is applied"
    );
    assert_eq!(settings.view(), Some(&view("Remote", "Blake")));
    assert_eq!(settings.value(Field::Assistant), "Later");
    assert_eq!(settings.value(Field::User), "Alex");
    assert!(
        settings.field(Field::Assistant).saving,
        "the rename still awaits an answer"
    );
    assert!(
        settings.field(Field::User).saving,
        "the user save still awaits an answer"
    );
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(7), 7, view("Remote", "Oscar"), Update::Answer),
        "the current answer settles the oldest save"
    );
    assert_eq!(settings.view(), Some(&view("Remote", "Oscar")));
    assert_eq!(settings.value(Field::User), "Oscar");
    assert_eq!(settings.value(Field::Assistant), "Later");
    assert!(
        !settings.field(Field::User).saving,
        "the first save settled"
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "the second save is pending"
    );
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(error.clone()))
    );
    assert_eq!(settings.notice(), Some(&error));
    assert!(
        settings.receive(Some(7), 7, view("Cloud", "Oscar"), Update::Answer),
        "the next current answer settles the inline rename"
    );
    assert_eq!(settings.view(), Some(&view("Cloud", "Oscar")));
    assert_eq!(settings.value(Field::Assistant), "Later");
    assert_eq!(settings.value(Field::User), "Oscar");
    assert!(
        !settings.field(Field::Assistant).saving,
        "the second save settled"
    );
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.notice(), Some(&error));
}
