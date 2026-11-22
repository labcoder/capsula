use super::{
    AssistantName, Completion, DockPosition, ErrorKey, Field, MaxFrameRate, Settings, SettingsOp,
    SettingsView, TaskbarChoice, Update, UserError, UserName,
};
use kumochi_protocol::message::ThreadId;

fn view() -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    }
}

#[test]
fn native_taskbar_refusal_keeps_the_saved_choice_and_reports_its_failure() {
    let mut state = Settings::default();
    state.apply_taskbar(|_| panic!("no snapshot to apply"));
    let mut saved = view();
    saved.show_in_taskbar = true;
    state.set_view(saved, Update::Unsolicited);
    let refusal = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(state.error(refusal.clone()), None);
    state.apply_taskbar(|choice| {
        assert_eq!(choice, TaskbarChoice::Shown);
        Some(crate::strings::Text::TaskbarAddFailed)
    });
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
    assert!(state.view().expect("saved snapshot").show_in_taskbar);
    assert_eq!(
        state.native_taskbar_notice,
        Some(crate::strings::Text::TaskbarAddFailed)
    );
    state.apply_taskbar(|choice| {
        assert_eq!(choice, TaskbarChoice::Shown);
        None
    });
    assert_eq!(state.native_taskbar_notice, None);
    assert_eq!(state.notice(), Some(&refusal));
    assert!(state.view().expect("saved snapshot").show_in_taskbar);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
}

#[test]
fn taskbar_admits_only_changed_online_choices_after_successful_send() {
    let mut state = Settings::default();
    assert_eq!(state.taskbar_choice(), None);
    assert!(!state.send_taskbar(TaskbarChoice::Shown, |_| panic!("offline send")));
    state.set_view(view(), Update::Unsolicited);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert!(!state.send_taskbar(TaskbarChoice::Hidden, |_| panic!("unchanged send")));
    assert!(!state.send_taskbar(TaskbarChoice::Shown, |_| false));
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    let mut sent = Vec::new();
    assert!(state.send_taskbar(TaskbarChoice::Shown, |op| {
        sent.push(op);
        true
    }));
    assert!(!state.send_taskbar(TaskbarChoice::Shown, |_| panic!("duplicate send")));
    assert!(state.send_taskbar(TaskbarChoice::Hidden, |op| {
        sent.push(op);
        true
    }));
    assert_eq!(
        sent,
        vec![SettingsOp::SetTaskbar(true), SettingsOp::SetTaskbar(false)]
    );
    assert_eq!(state.pending.len(), 2);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    let mut shown = view();
    shown.show_in_taskbar = true;
    state.set_view(shown, Update::Answer);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert_eq!(state.pending.len(), 1);
    state.set_view(view(), Update::Answer);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn taskbar_answers_share_the_fifo_without_completing_name_saves() {
    let mut state = Settings::default();
    state.set_view(view(), Update::Unsolicited);
    state.send_dock_position(DockPosition::BottomLeft, |_| true);
    assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
    state.change(Field::User, "Alex");
    assert!(state.save(Field::User, |_| true));
    assert_eq!(state.pending.len(), 3);
    assert_eq!(state.set_view(view(), Update::Answer), None);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
    assert!(state.field(Field::User).saving);
    assert_eq!(state.set_view(view(), Update::Answer), None);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert!(state.field(Field::User).saving);
    assert_eq!(state.pending.len(), 1);
    let mut saved = view();
    saved.user_name = Some(UserName::new("Alex").expect("valid"));
    assert_eq!(
        state.set_view(saved, Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::User,
        })
    );
    assert!(!state.field(Field::User).saving);
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn taskbar_refusal_stays_failed_and_only_a_taskbar_success_clears_its_notice() {
    let mut state = Settings::default();
    state.set_view(view(), Update::Unsolicited);
    assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
    let error = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(state.error(error.clone()), None);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    state.set_view(view(), Update::Unsolicited);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.notice(), Some(&error));
    state.set_view(view(), Update::Answer);
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.notice(), Some(&error));

    state.send_dock_position(DockPosition::BottomRight, |_| true);
    assert!(state.send_max_frame_rate(MaxFrameRate::cap(30).expect("cap"), |_| true));
    state.change(Field::User, "Alex");
    assert!(state.save(Field::User, |_| true));
    let mut saved = view();
    saved.dock_position = DockPosition::BottomRight;
    state.set_view(saved.clone(), Update::Answer);
    assert_eq!(state.notice(), Some(&error));
    saved.max_frame_rate = MaxFrameRate::cap(30).expect("cap");
    state.set_view(saved.clone(), Update::Answer);
    assert_eq!(state.notice(), Some(&error));
    saved.user_name = Some(UserName::new("Alex").expect("valid"));
    state.set_view(saved.clone(), Update::Answer);
    assert_eq!(state.notice(), Some(&error));

    assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
    state.set_view(saved.clone(), Update::Unsolicited);
    assert_eq!(state.notice(), Some(&error));
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
    saved.show_in_taskbar = true;
    state.set_view(saved, Update::Answer);
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
    assert_eq!(state.notice(), None);
}

#[test]
fn taskbar_success_preserves_dock_frame_rate_and_name_refusals() {
    for setting in [
        SettingsOp::SetDockPosition(DockPosition::BottomLeft),
        SettingsOp::SetMaxFrameRate(MaxFrameRate::cap(30).expect("cap")),
        SettingsOp::SetUserName(Some(UserName::new("Alex").expect("valid"))),
    ] {
        let mut state = Settings::default();
        state.set_view(view(), Update::Unsolicited);
        match setting {
            SettingsOp::SetDockPosition(position) => state.send_dock_position(position, |_| true),
            SettingsOp::SetMaxFrameRate(rate) => {
                assert!(state.send_max_frame_rate(rate, |_| true));
            }
            SettingsOp::SetUserName(_) => {
                state.change(Field::User, "Alex");
                assert!(state.save(Field::User, |_| true));
            }
            _ => panic!("unexpected fixture"),
        }
        let error = UserError::from(ErrorKey::SettingsRefused);
        state.error(error.clone());
        state.set_view(view(), Update::Answer);
        assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
        let mut saved = view();
        saved.show_in_taskbar = true;
        state.set_view(saved, Update::Answer);
        assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
        assert_eq!(state.notice(), Some(&error));
    }
}

#[test]
fn taskbar_disconnect_discards_intent_and_generation_fences_answers() {
    let mut state = Settings::default();
    assert!(state.receive(Some(7), 7, view(), Update::Unsolicited));
    assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
    for (current, generation) in [(Some(7), 6), (None, 7)] {
        assert!(!state.receive(current, generation, view(), Update::Answer));
        assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
        assert_eq!(state.pending.len(), 1);
    }
    state.disconnected();
    assert_eq!(state.taskbar_choice(), None);
    assert_eq!(state.view(), None);
    assert_eq!(state.pending.len(), 0);
    assert!(!state.send_taskbar(TaskbarChoice::Shown, |_| panic!("offline replay")));
    let mut shown = view();
    shown.show_in_taskbar = true;
    assert!(!state.receive(Some(8), 7, shown, Update::Answer));
    assert_eq!(state.taskbar_choice(), None);
    assert!(state.receive(Some(8), 8, view(), Update::Unsolicited));
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Hidden));
    assert_eq!(state.pending.len(), 0);
    assert!(state.send_taskbar(TaskbarChoice::Shown, |_| true));
    assert_eq!(state.taskbar_choice(), Some(TaskbarChoice::Shown));
}
