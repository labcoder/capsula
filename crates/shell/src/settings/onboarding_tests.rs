use kumochi_protocol::message::{
    AssistantName, DockPosition, ErrorKey, MaxFrameRate, OnboardingStep, SettingsOp, SettingsView,
    SttUnload, ThreadId, UserError,
};

use super::{Completion, Field, Settings, Update};

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a fixture's onboarded flag reads plainest as a bool"
)]
fn view(assistant: &str, onboarded: bool) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
        user_name: None,
        onboarded,
        onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: SttUnload::Kept,
        hold_to_talk: true,
    }
}

fn connected() -> Settings {
    let mut settings = Settings::default();
    assert_eq!(
        settings.set_view(view("Kumo", false), Update::Unsolicited),
        None
    );
    settings
}

#[test]
fn a_finish_queues_once_until_its_answer() {
    let mut settings = connected();
    let mut sent = Vec::new();
    assert!(settings.finish_onboarding(|op| {
        sent.push(op);
        true
    }));
    assert_eq!(sent, [SettingsOp::FinishOnboarding]);
    assert!(settings.finishing());
    assert!(!settings.finish_onboarding(|_| panic!("a second finish was sent")));
    assert_eq!(settings.set_view(view("Kumo", true), Update::Answer), None);
    assert!(!settings.finishing());
}

#[test]
fn a_finish_behind_a_name_save_is_answered_after_it() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Mochi");
    assert!(settings.save(Field::Assistant, |_| true));
    assert!(settings.finish_onboarding(|_| true));
    assert_eq!(
        settings.set_view(view("Mochi", false), Update::Answer),
        Some(Completion {
            failed: false,
            field: Field::Assistant
        })
    );
    assert!(settings.finishing(), "the first answer was the name's");
    assert!(!settings.field(Field::Assistant).saving);
    assert_eq!(settings.set_view(view("Mochi", true), Update::Answer), None);
    assert!(!settings.finishing());
}

#[test]
fn a_refused_finish_keeps_its_notice_until_a_later_finish_is_answered() {
    let mut settings = connected();
    assert!(settings.finish_onboarding(|_| true));
    let refused = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(settings.error(refused.clone()), None);
    assert_eq!(settings.finish_refusal(), Some(&refused));
    assert_eq!(settings.set_view(view("Kumo", false), Update::Answer), None);
    assert!(!settings.finishing());
    assert_eq!(settings.finish_refusal(), Some(&refused));
    assert!(settings.finish_onboarding(|_| true));
    assert_eq!(settings.set_view(view("Kumo", true), Update::Answer), None);
    assert_eq!(settings.finish_refusal(), None);
    assert_eq!(settings.notice(), None);
}

/// Only a record at the front of the queue is swallowed: a refused name save ahead of one still
/// raises its notice.
#[test]
fn a_refused_save_ahead_of_a_step_record_keeps_its_notice() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Mochi");
    assert!(settings.save(Field::Assistant, |_| true));
    assert!(settings.complete_onboarding_step(OnboardingStep::Welcome, |_| true));
    let refused = UserError::from(ErrorKey::SettingsRefused);
    assert_eq!(settings.error(refused.clone()), Some(Field::Assistant));
    assert_eq!(settings.notice(), Some(&refused));
}

#[test]
fn a_step_record_ahead_of_the_finish_is_never_taken_for_it_even_refused() {
    let mut settings = connected();
    let mut sent = Vec::new();
    assert!(
        !Settings::default().complete_onboarding_step(OnboardingStep::Welcome, |_| {
            panic!("sent while offline")
        })
    );
    assert!(!settings.complete_onboarding_step(OnboardingStep::Welcome, |_| false));
    assert!(
        settings.complete_onboarding_step(OnboardingStep::YourName, |op| {
            sent.push(op);
            true
        })
    );
    assert!(!settings.finishing());
    assert!(settings.finish_onboarding(|op| {
        sent.push(op);
        true
    }));
    assert_eq!(
        sent,
        [
            SettingsOp::CompleteOnboardingStep(OnboardingStep::YourName),
            SettingsOp::FinishOnboarding
        ]
    );
    assert_eq!(settings.error(ErrorKey::SettingsRefused.into()), None);
    assert_eq!(settings.set_view(view("Kumo", false), Update::Answer), None);
    assert_eq!(settings.notice(), None);
    assert_eq!(settings.finish_refusal(), None);
    assert!(settings.finishing());
    // Only the record at the front is swallowed: the finish's own refusal still shows.
    assert_eq!(settings.error(ErrorKey::SettingsRefused.into()), None);
    assert_eq!(
        settings.finish_refusal().map(UserError::key),
        Some(ErrorKey::SettingsRefused)
    );
    assert_eq!(settings.set_view(view("Kumo", false), Update::Answer), None);
    assert!(!settings.finishing());
}

#[test]
fn a_name_refusal_isnt_a_finish_refusal() {
    let mut settings = connected();
    settings.change(Field::Assistant, "Mochi");
    assert!(settings.save(Field::Assistant, |_| true));
    assert_eq!(
        settings.error(ErrorKey::SettingsRefused.into()),
        Some(Field::Assistant)
    );
    assert_eq!(settings.finish_refusal(), None);
}

#[test]
fn offline_or_unsent_queues_nothing_and_can_be_pressed_again() {
    let mut offline = Settings::default();
    assert!(!offline.finish_onboarding(|_| panic!("sent while offline")));
    assert!(!offline.finishing());
    let mut settings = connected();
    assert!(!settings.finish_onboarding(|_| false));
    assert!(!settings.finishing());
    assert!(settings.finish_onboarding(|_| true));
    assert!(settings.finishing());
    settings.disconnected();
    assert!(!settings.finishing());
}
