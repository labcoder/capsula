use kumochi_protocol::message::{
    AssistantName, DockPosition, ErrorKey, MaxFrameRate, NameError, SettingsOp, SettingsView,
    SttUnload, ThreadId, UserName,
};

use super::{Landing, Next, Onboarding, Step};
use crate::settings::{Error, Field, Settings, Update};

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a fixture's onboarded flag reads plainest as a bool"
)]
fn view(assistant: &str, user: Option<&str>, onboarded: bool) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
        user_name: user.map(|name| UserName::new(name).expect("valid user name")),
        onboarded,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: SttUnload::Kept,
    }
}

fn first_run(user: Option<&str>) -> (Settings, Onboarding) {
    let first = view("Kumo", user, false);
    let onboarding = Onboarding::begin(&first).expect("onboarding is owed");
    let mut settings = Settings::default();
    assert_eq!(settings.set_view(first, Update::Unsolicited), None);
    (settings, onboarding)
}

/// Continue or Enter as the window runs it: save the step's field, then submit.
fn press(settings: &mut Settings, onboarding: &mut Onboarding) -> (Vec<SettingsOp>, Next) {
    let mut sent = Vec::new();
    settings.save(onboarding.step().field(), |op| {
        sent.push(op);
        true
    });
    (sent, onboarding.submit(settings))
}

fn answer(settings: &mut Settings, onboarding: &mut Onboarding, view: SettingsView) -> Next {
    settings.set_view(view, Update::Answer);
    onboarding.settings_changed(settings)
}

fn on_your_name(user: Option<&str>) -> (Settings, Onboarding) {
    let (mut settings, mut onboarding) = first_run(user);
    assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Moved));
    assert_eq!(onboarding.step(), Step::YourName);
    (settings, onboarding)
}

#[test]
fn begin_is_owed_only_until_onboarded_and_an_onboarded_view_closes_at_any_step() {
    assert!(Onboarding::begin(&view("Kumo", None, true)).is_none());
    let (mut settings, mut onboarding) = first_run(None);
    assert_eq!(onboarding.step(), Step::Welcome);
    assert!(!onboarding.finishing());
    settings.set_view(view("Kumo", None, true), Update::Unsolicited);
    assert_eq!(
        onboarding.settings_changed(&settings),
        Next::Close(Landing::Quiet)
    );
    let (mut settings, mut onboarding) = on_your_name(None);
    settings.set_view(view("Kumo", None, true), Update::Unsolicited);
    assert_eq!(
        onboarding.settings_changed(&settings),
        Next::Close(Landing::Quiet)
    );
}

#[test]
fn a_changed_name_saves_once_and_goes_on_only_at_its_answer() {
    let (mut settings, mut onboarding) = first_run(None);
    settings.change(Field::Assistant, "Mochi");
    assert_eq!(
        press(&mut settings, &mut onboarding),
        (
            vec![SettingsOp::SetAssistantName(
                AssistantName::new("Mochi").expect("valid")
            )],
            Next::Stay
        )
    );
    assert_eq!(onboarding.step(), Step::Welcome);
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Mochi", None, false)),
        Next::Moved
    );
    assert_eq!(onboarding.step(), Step::YourName);
}

#[test]
fn a_refusal_stays_with_the_saved_name_and_continue_then_goes_on_without_sending() {
    let (mut settings, mut onboarding) = first_run(None);
    settings.change(Field::Assistant, "Mochi");
    assert_eq!(press(&mut settings, &mut onboarding).1, Next::Stay);
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::Assistant)
    );
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Kumo", None, false)),
        Next::Stay
    );
    assert_eq!(onboarding.step(), Step::Welcome);
    assert_eq!(settings.value(Field::Assistant), "Kumo");
    assert_eq!(
        settings.field(Field::Assistant).error,
        Some(Error::Brain(ErrorKey::SettingsRefused.into()))
    );
    assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Moved));
}

#[test]
fn invalid_names_stay_with_their_exact_error_and_send_nothing() {
    for (value, error) in [
        ("", NameError::Empty),
        ("   ", NameError::Empty),
        ("abcdefghijklmnopqrstuvwxy", NameError::TooLong),
        ("Ku\nmo", NameError::NotOneLine),
        ("Ku\u{2028}mo", NameError::NotOneLine),
    ] {
        let (mut settings, mut onboarding) = first_run(None);
        settings.change(Field::Assistant, value);
        assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Stay));
        assert_eq!(onboarding.step(), Step::Welcome);
        assert_eq!(
            settings.field(Field::Assistant).error,
            Some(Error::Validation(error))
        );
    }
}

#[test]
fn boundary_names_are_sent_trimmed() {
    for (value, sent) in [
        ("K", "K"),
        ("abcdefghijklmnopqrstuvwx", "abcdefghijklmnopqrstuvwx"),
        ("  Mochi  ", "Mochi"),
    ] {
        let (mut settings, mut onboarding) = first_run(None);
        settings.change(Field::Assistant, value);
        assert_eq!(
            press(&mut settings, &mut onboarding),
            (
                vec![SettingsOp::SetAssistantName(
                    AssistantName::new(sent).expect("valid")
                )],
                Next::Stay
            )
        );
        assert_eq!(
            answer(&mut settings, &mut onboarding, view(sent, None, false)),
            Next::Moved
        );
    }
}

#[test]
fn a_blank_name_with_nothing_saved_finishes_without_sending() {
    let (mut settings, mut onboarding) = on_your_name(None);
    settings.change(Field::User, "  ");
    assert_eq!(
        press(&mut settings, &mut onboarding),
        (vec![], Next::Finish)
    );
}

#[test]
fn a_blank_name_over_a_saved_one_clears_it_before_finishing() {
    let (mut settings, mut onboarding) = on_your_name(Some("Oscar"));
    settings.change(Field::User, "");
    assert_eq!(
        press(&mut settings, &mut onboarding),
        (vec![SettingsOp::SetUserName(None)], Next::Stay)
    );
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Kumo", None, false)),
        Next::Finish
    );
}

#[test]
fn continue_twice_during_one_save_sends_once_and_goes_on_once() {
    let (mut settings, mut onboarding) = first_run(None);
    settings.change(Field::Assistant, "Mochi");
    assert_eq!(press(&mut settings, &mut onboarding).0.len(), 1);
    assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Stay));
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Mochi", None, false)),
        Next::Moved
    );
    assert_eq!(
        onboarding.settings_changed(&settings),
        Next::Stay,
        "a later change doesn't go on again"
    );
    assert_eq!(onboarding.step(), Step::YourName);
}

#[test]
fn continue_after_a_blur_save_waits_for_that_answer() {
    let (mut settings, mut onboarding) = first_run(None);
    settings.change(Field::Assistant, "Mochi");
    let mut sent = Vec::new();
    assert!(settings.save(Field::Assistant, |op| {
        sent.push(op);
        true
    }));
    assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Stay));
    assert_eq!(sent.len(), 1);
    assert_eq!(onboarding.step(), Step::Welcome);
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Mochi", None, false)),
        Next::Moved
    );
}

#[test]
fn without_a_view_continue_does_nothing() {
    for value in ["Kumo", "Mochi"] {
        let (mut settings, mut onboarding) = first_run(None);
        settings.disconnected();
        settings.change(Field::Assistant, value);
        assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Stay));
        assert_eq!(onboarding.step(), Step::Welcome);
    }
}

#[test]
fn a_disconnect_forgets_the_wait_and_the_step_stays_after_reconnect() {
    let (mut settings, mut onboarding) = first_run(None);
    settings.change(Field::Assistant, "Mochi");
    assert_eq!(press(&mut settings, &mut onboarding).1, Next::Stay);
    settings.disconnected();
    assert_eq!(onboarding.settings_changed(&settings), Next::Stay);
    settings.set_view(view("Kumo", None, false), Update::Unsolicited);
    assert_eq!(onboarding.settings_changed(&settings), Next::Stay);
    assert_eq!(onboarding.step(), Step::Welcome);
    assert_eq!(settings.value(Field::Assistant), "Mochi");
}

#[test]
fn an_asked_finish_lands_on_the_peek_and_a_refused_one_can_be_asked_again() {
    let (mut settings, mut onboarding) = on_your_name(None);
    assert_eq!(press(&mut settings, &mut onboarding).1, Next::Finish);
    assert!(settings.finish_onboarding(|op| op == SettingsOp::FinishOnboarding));
    onboarding.finish_queued(Landing::Peek);
    assert!(onboarding.finishing());
    assert_eq!(press(&mut settings, &mut onboarding), (vec![], Next::Stay));
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        None,
        "a finish has no field"
    );
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Kumo", None, false)),
        Next::Stay
    );
    assert!(!onboarding.finishing());
    assert_eq!(press(&mut settings, &mut onboarding).1, Next::Finish);
    assert!(settings.finish_onboarding(|_| true));
    onboarding.finish_queued(Landing::Peek);
    assert_eq!(
        answer(&mut settings, &mut onboarding, view("Kumo", None, true)),
        Next::Close(Landing::Peek)
    );
}

#[test]
fn a_disconnect_forgets_a_queued_finish() {
    let (mut settings, mut onboarding) = on_your_name(None);
    assert!(settings.finish_onboarding(|_| true));
    onboarding.finish_queued(Landing::Peek);
    settings.disconnected();
    assert_eq!(onboarding.settings_changed(&settings), Next::Stay);
    assert!(!onboarding.finishing());
    settings.set_view(view("Kumo", None, false), Update::Unsolicited);
    assert_eq!(press(&mut settings, &mut onboarding).1, Next::Finish);
}
