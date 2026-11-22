//! OSC-906: onboarding's flow as the shell runs it: link drops at each step, a finish across a
//! reconnect, the couldn't-send notice, and how the window goes.

use kumochi_protocol::message::{
    AssistantName, DockPosition, MaxFrameRate, OnboardingStep, SettingsView, SttUnload, ThreadId,
};

use super::{Exit, Landing, Leave, Next, Onboarding, Step, ThisRun};
use crate::settings::{Field, Settings, Update};

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a fixture's onboarded flag reads plainest as a bool"
)]
fn view(from: OnboardingStep, onboarded: bool) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid assistant name"),
        user_name: None,
        onboarded,
        onboarding_from: from,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: SttUnload::Kept,
        hold_to_talk: true,
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
    }
}

fn at(from: OnboardingStep) -> (Settings, Onboarding) {
    let owed = view(from, false);
    let onboarding = Onboarding::begin(&owed, ThisRun::Owed).expect("onboarding is owed");
    let mut settings = Settings::default();
    assert_eq!(settings.set_view(owed, Update::Unsolicited), None);
    (settings, onboarding)
}

#[test]
fn a_link_drop_at_any_step_keeps_the_step_and_continue_goes_on_once_the_view_is_back() {
    for (from, step, then) in [
        (OnboardingStep::Welcome, Step::Welcome, Next::Moved),
        (OnboardingStep::YourName, Step::YourName, Next::Moved),
        (OnboardingStep::Accounts, Step::Accounts, Next::Moved),
        (
            OnboardingStep::Shortcut,
            Step::Shortcut,
            Next::Finish(Landing::Peek),
        ),
    ] {
        let (mut settings, mut onboarding) = at(from);
        if let Some(field) = step.field() {
            settings.change(field, "Mochi");
            assert!(settings.save(field, |_| true), "{from:?}");
            assert_eq!(onboarding.submit(&settings), Next::Stay, "{from:?}");
        }
        settings.disconnected();
        assert_eq!(
            onboarding.settings_changed(&settings),
            Next::Stay,
            "{from:?}"
        );
        assert_eq!(onboarding.submit(&settings), Next::Stay, "{from:?}");
        settings.set_view(view(from, false), Update::Unsolicited);
        assert_eq!(
            onboarding.settings_changed(&settings),
            Next::Stay,
            "{from:?}"
        );
        assert_eq!(onboarding.step(), step, "{from:?}");
        if let Some(field) = step.field() {
            settings.cancel(field);
        }
        assert_eq!(onboarding.submit(&settings), then, "{from:?}");
    }
}

#[test]
fn a_finish_dropped_with_the_link_is_asked_again_once_it_is_back_and_lands_on_onboarded() {
    let (mut settings, mut onboarding) = at(OnboardingStep::Shortcut);
    assert_eq!(onboarding.submit(&settings), Next::Finish(Landing::Peek));
    assert!(settings.finish_onboarding(|_| true));
    onboarding.finish_queued(Landing::Peek);
    assert!(onboarding.finishing());
    assert_eq!(onboarding.submit(&settings), Next::Stay);
    settings.disconnected();
    assert_eq!(onboarding.settings_changed(&settings), Next::Stay);
    assert!(!onboarding.finishing());
    settings.set_view(view(OnboardingStep::Shortcut, false), Update::Unsolicited);
    assert_eq!(onboarding.settings_changed(&settings), Next::Stay);
    assert_eq!(onboarding.submit(&settings), Next::Finish(Landing::Peek));
    assert!(settings.finish_onboarding(|_| true));
    onboarding.finish_queued(Landing::Peek);
    settings.set_view(view(OnboardingStep::Shortcut, true), Update::Answer);
    assert_eq!(
        onboarding.settings_changed(&settings),
        Next::Close(Landing::Peek)
    );
}

#[test]
fn a_finish_that_could_not_be_sent_says_so_until_the_next_continue_online() {
    let (mut settings, mut onboarding) = at(OnboardingStep::Shortcut);
    onboarding.finish_unsent();
    assert!(onboarding.unsent());
    assert!(!onboarding.finishing());
    assert_eq!(onboarding.submit(&settings), Next::Finish(Landing::Peek));
    assert!(!onboarding.unsent());
    onboarding.finish_queued(Landing::Typing);
    assert!(!onboarding.unsent());
    assert!(onboarding.finishing());
    let (_, mut onboarding) = at(OnboardingStep::Shortcut);
    onboarding.finish_unsent();
    settings.disconnected();
    assert_eq!(onboarding.submit(&settings), Next::Stay);
    assert!(onboarding.unsent());
}

#[test]
fn the_window_goes_in_the_order_the_shell_runs() {
    let (mut settings, onboarding) = at(OnboardingStep::Welcome);
    assert_eq!(
        onboarding.leave(Exit::Dismiss, &settings),
        [Leave::CancelSignIns, Leave::PutOff, Leave::Close]
    );
    settings.change(Field::Assistant, "Mochi");
    assert_eq!(
        onboarding.leave(Exit::Dismiss, &settings),
        [
            Leave::Save(Field::Assistant),
            Leave::CancelEdit,
            Leave::CancelSignIns,
            Leave::PutOff,
            Leave::Close
        ]
    );
    assert_eq!(
        onboarding.leave(Exit::Close(Landing::Typing), &settings),
        [
            Leave::CancelEdit,
            Leave::HandOverSignIns,
            Leave::Close,
            Leave::Land(Landing::Typing)
        ]
    );
    let (settings, onboarding) = at(OnboardingStep::Accounts);
    assert_eq!(
        onboarding.leave(Exit::Dismiss, &settings),
        [Leave::CancelSignIns, Leave::PutOff, Leave::Close]
    );
    assert_eq!(
        onboarding.leave(Exit::Close(Landing::Peek), &settings),
        [
            Leave::HandOverSignIns,
            Leave::Close,
            Leave::Land(Landing::Peek)
        ]
    );
}

#[test]
fn only_leaving_the_account_step_hands_its_sign_ins_over() {
    for (step, hands_over) in [
        (Step::Welcome, false),
        (Step::YourName, false),
        (Step::Accounts, true),
        (Step::Shortcut, false),
    ] {
        assert_eq!(step.hands_over_sign_ins(), hands_over, "{step:?}");
    }
}
