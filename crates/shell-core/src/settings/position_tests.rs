use super::*;
use kumochi_protocol::message::ThreadId;

fn view(position: DockPosition) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid"),
        user_name: None,
        onboarded: true,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: position,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: kumochi_protocol::message::SttUnload::Kept,
        hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    }
}

#[test]
fn offline_picks_coalesce_and_flush_once_after_admission() {
    let mut state = Settings::default();
    assert_eq!(state.dock_position_choice(), None);
    state.send_dock_position(DockPosition::BottomLeft, |_| panic!("offline send"));
    state.send_dock_position(DockPosition::BottomRight, |_| panic!("offline send"));
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomRight)
    );
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert!(!state.flush_offline_position(|_| false));
    assert_eq!(state.pending.len(), 0);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomRight)
    );
    let mut sent = Vec::new();
    assert!(state.flush_offline_position(|op| {
        sent.push(op);
        true
    }));
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert!(!state.flush_offline_position(|_| panic!("duplicate send")));
    assert_eq!(
        sent,
        vec![SettingsOp::SetDockPosition(DockPosition::BottomRight)]
    );
    assert_eq!(state.pending.len(), 1);
    state.set_view(view(DockPosition::BottomRight), Update::Answer);
    assert_eq!(state.pending.len(), 0);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomRight)
    );
}

#[test]
fn newest_choice_survives_older_answers_and_disconnects() {
    let mut state = Settings::default();
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    state.send_dock_position(DockPosition::MiddleLeft, |_| true);
    state.send_dock_position(DockPosition::Center, |_| true);
    state.set_view(view(DockPosition::MiddleLeft), Update::Answer);
    assert_eq!(state.dock_position_choice(), Some(DockPosition::Center));
    state.send_dock_position(DockPosition::BottomCenter, |_| false);
    state.set_view(view(DockPosition::Center), Update::Answer);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomCenter)
    );
    state.disconnected();
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomCenter)
    );
    assert!(!state.receive(
        Some(9),
        8,
        view(DockPosition::TopRight),
        Update::Unsolicited
    ));
    assert_eq!(state.view(), None);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomCenter)
    );
    assert!(state.receive(
        Some(9),
        9,
        view(DockPosition::TopRight),
        Update::Unsolicited
    ));
    assert!(state.flush_offline_position(|op| {
        assert_eq!(op, SettingsOp::SetDockPosition(DockPosition::BottomCenter));
        true
    }));
    state.disconnected();
    state.set_view(view(DockPosition::TopRight), Update::Unsolicited);
    assert!(state.flush_offline_position(|op| {
        assert_eq!(op, SettingsOp::SetDockPosition(DockPosition::BottomCenter));
        true
    }));
    state.disconnected();
    state.set_view(view(DockPosition::BottomCenter), Update::Unsolicited);
    assert!(!state.flush_offline_position(|_| panic!("already persisted")));
    assert_eq!(state.pending.len(), 0);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::BottomCenter)
    );
}

#[test]
fn unsent_newer_pick_wins_until_a_newer_admitted_pick_replaces_it() {
    let mut state = Settings::default();
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    state.send_dock_position(DockPosition::MiddleLeft, |_| true);
    state.send_dock_position(DockPosition::Center, |_| false);
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert_eq!(state.dock_position_choice(), Some(DockPosition::Center));
    assert_eq!(state.pending.len(), 1);
    state.send_dock_position(DockPosition::MiddleRight, |_| true);
    assert!(state.offline_position.is_none());
    state.set_view(view(DockPosition::MiddleLeft), Update::Answer);
    assert_eq!(
        state.dock_position_choice(),
        Some(DockPosition::MiddleRight)
    );
    assert_eq!(state.pending.len(), 1);
}

#[test]
fn an_explicit_default_pick_still_sends_but_a_refused_pick_never_replays() {
    let mut state = Settings::default();
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    let mut sent = Vec::new();
    state.send_dock_position(DockPosition::TopCenter, |op| {
        sent.push(op);
        true
    });
    assert_eq!(
        sent,
        vec![SettingsOp::SetDockPosition(DockPosition::TopCenter)]
    );
    state.error(ErrorKey::SettingsRefused.into());
    state.set_view(view(DockPosition::TopLeft), Update::Answer);
    assert_eq!(state.dock_position_choice(), Some(DockPosition::TopLeft));
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    state.disconnected();
    state.set_view(view(DockPosition::TopLeft), Update::Unsolicited);
    assert!(!state.flush_offline_position(|_| panic!("refusal is final")));
    assert_eq!(state.dock_position_choice(), Some(DockPosition::TopLeft));
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
}

#[test]
fn a_successful_different_value_clears_only_its_own_settings_refusal() {
    let mut state = Settings::default();
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert!(state.send_max_frame_rate(MaxFrameRate::cap(30).expect("cap"), |_| true));
    state.error(ErrorKey::SettingsRefused.into());
    state.set_view(view(DockPosition::TopCenter), Update::Answer);
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert!(state.send_max_frame_rate(MaxFrameRate::cap(60).expect("cap"), |_| true));
    let mut saved = view(DockPosition::TopCenter);
    saved.max_frame_rate = MaxFrameRate::cap(60).expect("cap");
    state.set_view(saved, Update::Answer);
    assert_eq!(state.notice(), None);
    state.send_dock_position(DockPosition::MiddleLeft, |_| true);
    state.error(ErrorKey::SettingsRefused.into());
    state.set_view(view(DockPosition::TopCenter), Update::Answer);
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    state.send_dock_position(DockPosition::BottomRight, |_| true);
    state.set_view(view(DockPosition::BottomRight), Update::Answer);
    assert_eq!(state.notice(), None);
}

#[test]
fn dock_answers_cannot_complete_name_saves_or_clear_frame_rate_refusals() {
    let mut state = Settings::default();
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert!(state.send_max_frame_rate(MaxFrameRate::cap(30).expect("cap"), |_| true));
    state.error(ErrorKey::SettingsRefused.into());
    state.send_dock_position(DockPosition::MiddleRight, |_| true);
    state.change(Field::User, "Alex");
    assert!(state.save(Field::User, |_| true));
    state.set_view(view(DockPosition::TopCenter), Update::Unsolicited);
    assert_eq!(state.pending.len(), 3);
    state.set_view(view(DockPosition::TopCenter), Update::Answer);
    state.set_view(view(DockPosition::MiddleRight), Update::Answer);
    assert!(state.field(Field::User).saving);
    assert_eq!(state.pending.len(), 1);
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    let mut saved = view(DockPosition::MiddleRight);
    saved.user_name = Some(UserName::new("Alex").expect("valid"));
    state.set_view(saved, Update::Answer);
    assert!(!state.field(Field::User).saving);
    assert_eq!(state.pending.len(), 0);
    assert_eq!(
        state.notice().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
}
