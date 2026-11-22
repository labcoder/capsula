//! OSC-700: closing the onboarding window removes it first, so a close that fails keeps the
//! window and the hold pause, and a later close still removes it.

use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
use kumochi_protocol::message::{
    AssistantName, DockPosition, MaxFrameRate, OnboardingStep, SettingsView, SttUnload, ThreadId,
};

use super::{OnboardingPanel, OnboardingView};
use crate::onboarding::{Onboarding, ThisRun};
use crate::presence::test_fixture;

fn owed() -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new("Kumo").expect("valid assistant name"),
        user_name: None,
        onboarded: false,
        onboarding_from: OnboardingStep::Welcome,
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

#[gpui::test]
fn a_close_that_fails_keeps_the_window_and_the_pause_and_a_later_close_removes_it(
    cx: &mut TestAppContext,
) {
    let owner = test_fixture::presence(cx);
    let window = owner.update(cx, |owner, cx| {
        let state = Onboarding::begin(&owed(), ThisRun::Owed).expect("onboarding is owed");
        let (model, events, weak) = (
            owner.settings.clone(),
            owner.events.clone(),
            cx.weak_entity(),
        );
        let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| OnboardingView::new(state, model, weak, events, window, cx))
        })
        .expect("a GPUI test window opens");
        owner.set_onboarding(
            OnboardingPanel {
                window,
                view,
                bounds: [0; 4],
                previous: None,
            },
            cx,
        );
        window
    });
    let inside = window
        .update(cx, |_, _, cx| {
            owner.update(cx, |owner, cx| owner.close_onboarding(cx).is_err())
        })
        .expect("the window is open");
    assert!(
        inside,
        "a window can't be removed from inside its own update"
    );
    owner.update(cx, |owner, _| {
        assert!(owner.onboarding.is_some());
        assert!(owner.voice.paused.get());
    });
    owner.update(cx, |owner, cx| owner.close_onboarding(cx).expect("close"));
    owner.update(cx, |owner, _| {
        assert!(owner.onboarding.is_none());
        assert!(!owner.voice.paused.get());
    });
    assert!(window.update(cx, |_, _, _| ()).is_err());
}
