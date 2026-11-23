use kumochi_protocol::message::{ErrorKey, MaxFrameRate, ThreadId};

use crate::strings::Text;

use super::{
    AssistantName, Completion, Error, Field, NameError, Settings, SettingsOp, SettingsView, Update,
    UserName,
};

fn view(assistant: &str, user: Option<&str>) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
        user_name: user.map(|name| UserName::new(name).expect("valid user name")),
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
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
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
#[expect(
    clippy::too_many_lines,
    reason = "all save paths share one FIFO and one disconnect, so the table and replay assertions belong together"
)]
fn controller_saves_keep_their_wire_operations_and_only_the_dock_move_replays_after_drop() {
    use super::{AfterSave, Save, SaveOutcome, TaskbarChoice};
    use crate::link::{LinkHandle, Session};
    use kumochi_protocol::message::{
        ConnectionKind, ControlToBrain, DockPosition, Effort, Incarnation, ModelChoice, ModelSlug,
        PartState, SttUnload,
    };

    let mut link = LinkHandle::detached();
    let controls = link.record_controls();
    let first = Session::detached(7, Incarnation(1));
    let second = Session::detached(8, Incarnation(1));
    let initial = view("Kumo", Some("Oscar"));
    let mut state = Settings::default();
    assert!(state.receive_and_replay_dock(
        Some((&link, &first)),
        7,
        initial.clone(),
        Update::Unsolicited
    ));
    assert!(state.receive_speech_input(Some(7), 7, PartState::Unloaded));
    let reload = state.speech_input_intent().expect("speech can load");
    state.change(Field::User, "Alex");
    let model = ModelChoice {
        connection: ConnectionKind::ChatGpt,
        model: ModelSlug::new("fixture").unwrap(),
        effort: Effort::Low,
    };
    let assistant = AssistantName::new("Cloud").unwrap();
    let rate = MaxFrameRate::cap(144).unwrap();
    for (save, op, after) in [
        (
            Save::Name(Field::User),
            SettingsOp::SetUserName(Some(UserName::new("Alex").unwrap())),
            None,
        ),
        (
            Save::RenameAssistant(assistant.clone()),
            SettingsOp::SetAssistantName(assistant),
            None,
        ),
        (
            Save::DefaultModel(model.connection, model.model.clone()),
            SettingsOp::SetDefaultModel(model),
            None,
        ),
        (
            Save::FrameRate(rate),
            SettingsOp::SetMaxFrameRate(rate),
            None,
        ),
        (
            Save::DockPosition(DockPosition::BottomLeft),
            SettingsOp::SetDockPosition(DockPosition::BottomLeft),
            None,
        ),
        (
            Save::Taskbar(TaskbarChoice::Shown),
            SettingsOp::SetTaskbar(true),
            Some(AfterSave::SyncTaskbar),
        ),
        (
            Save::HoldToTalk(false),
            SettingsOp::SetHoldToTalk(false),
            Some(AfterSave::FollowAndCancelVoice),
        ),
        (Save::TalkBack(true), SettingsOp::SetTalkBack(true), None),
        (
            Save::ShowScreen(false),
            SettingsOp::SetShowScreen(false),
            Some(AfterSave::RefreshScreenOffer { clear_marks: true }),
        ),
        (
            Save::SttUnload(SttUnload::After15Min),
            SettingsOp::SetSttUnload(SttUnload::After15Min),
            None,
        ),
        (Save::SpeechInput(reload), SettingsOp::LoadSpeechInput, None),
    ] {
        assert_eq!(
            state.apply_save(save, Some((&link, &first))),
            SaveOutcome {
                result: Ok(true),
                notify: true,
                after
            }
        );
        assert_eq!(controls.try_recv(), Ok(ControlToBrain::Settings(op)));
    }
    assert!(state.field(Field::User).saving);
    assert!(state.field(Field::Assistant).saving);
    assert_eq!(state.max_frame_rate_choice(), Some(rate));
    assert!(state.model_saving() && state.hold_to_talk_pending() && state.talk_back_pending());
    assert!(
        state.show_screen_pending() && state.stt_unload_pending() && state.speech_input_pending()
    );

    state.disconnected();
    assert!(!state.receive_and_replay_dock(
        Some((&link, &second)),
        7,
        initial.clone(),
        Update::Answer
    ));
    assert!(controls.is_empty());
    assert_eq!(state.view(), None);
    assert!(state.receive_and_replay_dock(
        Some((&link, &second)),
        8,
        initial.clone(),
        Update::Unsolicited
    ));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::SetDockPosition(
            DockPosition::BottomLeft
        )))
    );
    assert!(controls.is_empty(), "only the dock preference can replay");
    assert!(!state.field(Field::User).saving && !state.field(Field::Assistant).saving);
    assert_eq!(
        state.value(Field::User),
        "Alex",
        "the user's draft survives"
    );
    assert_eq!(state.max_frame_rate_choice(), Some(initial.max_frame_rate));
    assert_eq!(state.default_model_choice(), None);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert_eq!(state.hold_to_talk_choice(), Some(true));
    assert_eq!(state.talk_back_choice(), Some(false));
    assert_eq!(state.show_screen_choice(), Some(true));
    assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
    assert_eq!(state.speech_input_intent(), None);
}

#[test]
fn a_failed_privacy_save_still_retires_the_screen_offer_and_marks() {
    use super::{AfterSave, BeforeSave, Save, SaveOutcome};

    let mut state = connected();
    let nope = Save::ShowScreen(false);
    assert_eq!(nope.before(), Some(BeforeSave::RevokeScreenOffer));
    assert_eq!(
        state.apply_save(nope, None),
        SaveOutcome {
            result: Ok(false),
            notify: true,
            after: Some(AfterSave::RefreshScreenOffer { clear_marks: true }),
        }
    );
    assert_eq!(
        state.show_screen_choice(),
        Some(true),
        "the save didn't land"
    );
}

#[test]
fn unchanged_normalized_names_never_save_even_with_an_error() {
    let mut settings = connected();
    for field in [Field::Assistant, Field::User] {
        assert!(
            !settings.save(field, |_| panic!("an unchanged field was sent")),
            "focus and blur do not write settings"
        );
        assert!(!settings.field(field).saving, "no save is pending");
        assert_eq!(settings.field(field).error, None);
    }
    for (field, value) in [(Field::Assistant, " Kumo "), (Field::User, " Oscar ")] {
        settings.change(field, value);
        assert!(
            !settings.save(field, |_| panic!("a normalized unchanged name was sent")),
            "whitespace does not make a name change"
        );
        assert!(
            !settings.field(field).saving,
            "no normalized save is pending"
        );
        assert_eq!(settings.field(field).error, None);
        assert!(
            !settings.editing(field),
            "a no-op save settles the draft, so later views and renames reach it"
        );
    }
    assert!(
        !settings.send_assistant(AssistantName::new(" Kumo ").expect("valid"), |_| panic!(
            "an unchanged inline rename was sent"
        )),
        "an inline rename also skips the saved name"
    );
    assert_eq!(settings.error(ErrorKey::SettingsRefused.into()), None);
    settings.change(Field::User, "Alex");
    assert!(settings.save(Field::User, |_| true), "the edit is sent");
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::User)
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: true,
            field: Field::User
        })
    );
    assert_eq!(settings.value(Field::User), "Oscar");
    assert!(
        !settings.editing(Field::User),
        "refusal restored a clean draft"
    );
    assert!(
        !settings.save(Field::User, |_| panic!("an error-present no-op was sent")),
        "an error does not turn an unchanged name into a change"
    );
    assert!(
        !settings.field(Field::User).saving,
        "the no-op stays unsent"
    );
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
}

#[test]
fn save_uses_shared_validation_and_settles_the_trimmed_name() {
    let mut settings = connected();
    settings.change(Field::Assistant, "   ");
    assert!(
        !settings.save(Field::Assistant, |_| panic!("invalid name sent")),
        "an invalid name stays local"
    );
    assert_eq!(
        settings.field(Field::Assistant).error,
        Some(Error::Validation(NameError::Empty))
    );
    settings.change(Field::Assistant, "  Cloud  ");
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert!(settings.editing(Field::Assistant), "the draft is dirty");
    assert!(
        settings.save(Field::Assistant, |op| {
            assert_eq!(
                op,
                SettingsOp::SetAssistantName(AssistantName::new("Cloud").expect("valid"))
            );
            true
        }),
        "a valid name is sent"
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "the save is pending"
    );
    assert_eq!(settings.value(Field::Assistant), "  Cloud  ");
    assert!(
        !settings.save(Field::Assistant, |_| panic!("duplicate save")),
        "one field cannot have two saves in flight"
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert!(
        !settings.field(Field::Assistant).saving,
        "the answer settles it"
    );
    assert!(
        !settings.editing(Field::Assistant),
        "the saved name is clean"
    );
    assert_eq!(settings.view(), Some(&view("Cloud", Some("Oscar"))));
}

#[test]
fn refusal_rolls_back_and_keeps_the_error_until_an_actual_change() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Cloud");
    assert!(
        settings.save(Field::Assistant, |_| true),
        "the save is sent"
    );
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::Assistant)
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: true,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Kumo");
    assert!(
        !settings.editing(Field::Assistant),
        "refusal restores the saved name"
    );
    let error = Some(Error::Brain(ErrorKey::SettingsRefused.into()));
    assert_eq!(settings.field(Field::Assistant).error, error);
    settings.change(Field::Assistant, "Kumo");
    settings.cancel(Field::Assistant);
    assert_eq!(settings.field(Field::Assistant).error, error);
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
        None
    );
    assert_eq!(settings.field(Field::Assistant).error, error);
    settings.change(Field::Assistant, "Cloud");
    assert_eq!(settings.field(Field::Assistant).error, None);
    settings.cancel(Field::Assistant);
    assert_eq!(settings.value(Field::Assistant), "Kumo");
    assert!(
        !settings.editing(Field::Assistant),
        "cancel restores the saved name"
    );
}

#[test]
fn empty_or_whitespace_user_names_clear_the_setting() {
    for value in ["", " \t\n "] {
        let mut settings = connected();
        settings.change(Field::User, value);
        assert!(
            settings.save(Field::User, |op| {
                assert_eq!(op, SettingsOp::SetUserName(None));
                true
            }),
            "an empty user name sends a clear"
        );
        assert_eq!(
            settings.set_view(view("Kumo", None), Update::Answer),
            Some(Completion {
                failed: false,
                field: Field::User
            })
        );
        assert_eq!(settings.value(Field::User), "");
        assert!(!settings.editing(Field::User), "the cleared name is clean");
        assert_eq!(settings.field(Field::User).error, None);
    }
}

#[test]
fn errors_and_answers_follow_both_fields_in_fifo_order() {
    let mut settings = connected();
    settings.change(Field::User, "Alex");
    assert!(
        settings.save(Field::User, |_| true),
        "the user save is sent first"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the inline assistant rename is sent second"
    );
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::User)
    );
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: true,
            field: Field::User
        })
    );
    assert!(
        !settings.field(Field::User).saving,
        "the first save is settled"
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "the second save is pending"
    );
    assert_eq!(settings.value(Field::User), "Oscar");
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        None
    );
}

#[test]
fn unsolicited_views_do_not_complete_saves_or_replace_drafts() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Cloud");
    assert!(
        settings.save(Field::Assistant, |_| true),
        "the save is sent"
    );
    assert_eq!(
        settings.set_view(view("Other", Some("Alex")), Update::Unsolicited),
        None
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "an unsolicited view is not an answer"
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(settings.value(Field::User), "Alex");
    assert_eq!(
        settings.set_view(view("Cloud", Some("Alex")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert!(
        !settings.field(Field::Assistant).saving,
        "the answer settles the save"
    );
}

#[test]
fn later_typing_survives_success_and_refusal() {
    for failed in [false, true] {
        let mut settings = connected();
        settings.change(Field::Assistant, "Cloud");
        assert!(
            settings.save(Field::Assistant, |_| true),
            "the save is sent"
        );
        if failed {
            assert_eq!(
                settings.error(ErrorKey::SettingsRefused.into()),
                Some(Field::Assistant)
            );
        }
        settings.change(Field::Assistant, "Later");
        let saved = if failed { "Kumo" } else { "Cloud" };
        assert_eq!(
            settings.set_view(view(saved, Some("Oscar")), Update::Answer),
            Some(Completion {
                failed,
                field: Field::Assistant
            })
        );
        assert_eq!(settings.value(Field::Assistant), "Later");
        assert!(
            settings.editing(Field::Assistant),
            "later typing remains dirty"
        );
        assert_eq!(settings.field(Field::Assistant).error, None);
        settings.cancel(Field::Assistant);
        assert_eq!(settings.value(Field::Assistant), saved);
    }
}

#[test]
fn reconnect_drops_old_fifo_and_preserves_errors() {
    let mut settings = connected();
    settings.change(Field::User, "Alex");
    assert!(
        settings.save(Field::User, |_| true),
        "the old connection accepted a save"
    );
    assert_eq!(
        settings.error(ErrorKey::ConfigUnreadable.into()),
        Some(Field::User)
    );
    settings.disconnected();
    assert_eq!(settings.view(), None);
    assert!(
        !settings.field(Field::User).saving,
        "disconnect drops the pending save"
    );
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::ConfigUnreadable.into()))
    );
    settings.change(Field::Assistant, "OfflineDraft");
    assert!(
        !settings.save(Field::Assistant, |_| panic!("offline send")),
        "saving is disabled until settings arrive"
    );
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
        None
    );
    settings.change(Field::Assistant, "Cloud");
    assert!(
        settings.save(Field::Assistant, |_| true),
        "the new connection accepts a save"
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::User), "Alex");
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::ConfigUnreadable.into()))
    );
}

#[test]
fn successful_retry_clears_only_its_brain_error() {
    let mut settings = connected();
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the first rename is sent"
    );
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::Assistant)
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: true,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Kumo");
    assert!(
        !settings.send_assistant(AssistantName::new("Kumo").expect("valid"), |_| panic!(
            "an error-present inline no-op was sent"
        )),
        "the error does not make the saved assistant name a change"
    );
    settings.change(Field::User, "Alex");
    assert!(
        !settings.save(Field::User, |_| false),
        "a rejected send is not queued"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the failed different name is retried without editing the settings draft"
    );
    assert_eq!(settings.value(Field::Assistant), "Kumo");
    assert_eq!(
        settings.field(Field::Assistant).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert_eq!(settings.field(Field::User).error, Some(Error::Offline));
    assert!(
        !settings.field(Field::User).saving,
        "a rejected send is never pending"
    );
}

#[test]
fn successful_retry_of_the_same_draft_clears_its_offline_error() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Cloud");
    assert!(
        !settings.save(Field::Assistant, |_| false),
        "the first send fails"
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    assert!(
        !settings.field(Field::Assistant).saving,
        "a failed send is not pending"
    );
    settings.disconnected();
    assert_eq!(settings.view(), None);
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Unsolicited),
        None
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    assert!(
        settings.save(Field::Assistant, |_| true),
        "the same dirty draft is retried after reconnect"
    );
    assert!(
        settings.field(Field::Assistant).saving,
        "the retry is pending"
    );
    assert_eq!(settings.field(Field::Assistant).error, Some(Error::Offline));
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Cloud");
    assert_eq!(settings.field(Field::Assistant).error, None);
    assert!(
        !settings.field(Field::Assistant).saving,
        "success settles the retry"
    );
    assert!(
        !settings.editing(Field::Assistant),
        "the saved draft is clean"
    );
}

#[test]
fn successful_inline_rename_preserves_an_invalid_settings_draft() {
    let mut settings = connected();
    settings.change(Field::Assistant, "");
    assert!(
        !settings.save(Field::Assistant, |_| panic!("invalid draft sent")),
        "the invalid settings draft stays local"
    );
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the independent inline rename is sent"
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "");
    assert_eq!(
        settings.field(Field::Assistant).error,
        Some(Error::Validation(NameError::Empty))
    );
    assert!(
        settings.editing(Field::Assistant),
        "the local draft remains dirty"
    );
    assert!(
        !settings.field(Field::Assistant).saving,
        "the inline rename settled"
    );
}

#[test]
fn inline_rename_preserves_an_existing_settings_draft() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Draft");
    assert!(
        settings.send_assistant(AssistantName::new("Cloud").expect("valid"), |_| true),
        "the inline rename is sent"
    );
    assert_eq!(
        settings.set_view(view("Cloud", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert_eq!(settings.value(Field::Assistant), "Draft");
    settings.cancel(Field::Assistant);
    assert_eq!(settings.value(Field::Assistant), "Cloud");
}

#[test]
fn debug_never_contains_names_or_drafts() {
    let mut settings = Settings::default();
    assert_eq!(
        settings.set_view(
            view("AssistantSentinel", Some("UserSentinel")),
            Update::Unsolicited
        ),
        None
    );
    settings.change(Field::Assistant, "DraftSentinel");
    assert!(
        settings.save(Field::Assistant, |_| true),
        "the draft is also queued"
    );
    let debug = format!("{settings:?} {:?}", settings.field(Field::Assistant));
    assert!(
        !debug.contains("AssistantSentinel"),
        "the saved assistant name is redacted"
    );
    assert!(
        !debug.contains("UserSentinel"),
        "the saved user name is redacted"
    );
    assert!(!debug.contains("DraftSentinel"), "the draft is redacted");
}

#[test]
fn a_frame_rate_answer_completes_no_field() {
    let mut settings = connected();
    let rate = MaxFrameRate::cap(144).expect("a cap");
    assert!(
        !settings.send_max_frame_rate(MaxFrameRate::AUTOMATIC, |_| panic!(
            "the current cap was sent"
        )),
        "the current cap isn't sent"
    );
    assert!(
        !settings.send_max_frame_rate(rate, |_| false),
        "an unsent cap queues nothing"
    );
    let mut sent = None;
    assert!(settings.send_max_frame_rate(rate, |op| {
        sent = Some(op);
        true
    }));
    assert_eq!(sent, Some(SettingsOp::SetMaxFrameRate(rate)));
    settings.change(Field::User, "Alex");
    assert!(settings.save(Field::User, |_| true));
    let mut answer = view("Kumo", Some("Oscar"));
    answer.max_frame_rate = rate;
    assert_eq!(settings.set_view(answer.clone(), Update::Answer), None);
    assert!(
        settings.field(Field::User).saving,
        "the frame rate's answer isn't the name's"
    );
    assert_eq!(settings.view().map(|view| view.max_frame_rate), Some(rate));
    assert_eq!(
        settings.set_view(answer, Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::User
        })
    );
    settings.disconnected();
    assert!(!settings.send_max_frame_rate(MaxFrameRate::AUTOMATIC, |_| panic!("sent offline")));
}

#[test]
fn a_second_frame_rate_choice_before_the_answer_is_sent() {
    let mut settings = connected();
    let rate = MaxFrameRate::cap(144).expect("a cap");
    let mut sent = Vec::new();
    for choice in [rate, MaxFrameRate::AUTOMATIC] {
        assert!(settings.send_max_frame_rate(choice, |op| {
            sent.push(op);
            true
        }));
    }
    assert!(
        !settings.send_max_frame_rate(MaxFrameRate::AUTOMATIC, |_| panic!(
            "the queued cap was sent"
        )),
        "the newest queued choice counts as current"
    );
    assert_eq!(
        sent,
        [
            SettingsOp::SetMaxFrameRate(rate),
            SettingsOp::SetMaxFrameRate(MaxFrameRate::AUTOMATIC)
        ]
    );
}

#[test]
fn a_frame_rate_refusal_marks_no_field_and_clears_on_its_retry() {
    let mut settings = connected();
    let rate = MaxFrameRate::cap(30).expect("a cap");
    assert!(settings.send_max_frame_rate(rate, |_| true));
    settings.change(Field::User, "Alex");
    assert!(settings.save(Field::User, |_| true));
    assert_eq!(settings.error(ErrorKey::SettingsRefused.into()), None);
    assert_eq!(settings.field(Field::User).error, None);
    assert_eq!(settings.dock_notice(), Some(Text::DockError));
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        None
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Alex")), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::User
        })
    );
    assert_eq!(
        settings.dock_notice(),
        Some(Text::DockError),
        "a name's answer isn't the retry"
    );
    assert!(settings.send_max_frame_rate(rate, |_| true));
    let mut answer = view("Kumo", Some("Alex"));
    answer.max_frame_rate = rate;
    assert_eq!(settings.set_view(answer, Update::Answer), None);
    assert_eq!(
        settings.dock_notice(),
        None,
        "the frame rate's own retry clears it"
    );
}

#[test]
fn a_name_refusal_ahead_of_a_frame_rate_stays_on_the_name() {
    let mut settings = connected();
    settings.change(Field::User, "Alex");
    assert!(settings.save(Field::User, |_| true));
    assert!(settings.send_max_frame_rate(MaxFrameRate::cap(60).expect("a cap"), |_| true));
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::User)
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        Some(Completion {
            failed: true,
            field: Field::User
        })
    );
    assert_eq!(
        settings.set_view(view("Kumo", Some("Oscar")), Update::Answer),
        None
    );
    assert_eq!(
        settings.field(Field::User).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
    assert_eq!(
        settings.dock_notice(),
        Some(Text::DockError),
        "the frame rate's answer keeps the name's notice"
    );
}
