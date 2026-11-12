use std::time::{Duration, Instant};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
    WindowOptions,
};
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, DockPosition, Incarnation, MaxFrameRate, SettingsOp,
    SettingsView, SttUnload, ThreadId,
};

use super::{OnboardingPanel, OnboardingView, Select};
use crate::input::{Edge, Effect, Event, Gesture, Key};
use crate::link::{Batch, LinkEvent, Session};
use crate::onboarding::{Onboarding, Step};
use crate::platform::{EventTime, PresenceEvent};
use crate::presence::{PanelVisibility, Presence, test_fixture};
use crate::settings::{Field, Update};

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a fixture's onboarded flag reads plainest as a bool"
)]
fn view(assistant: &str, onboarded: bool) -> SettingsView {
    SettingsView {
        assistant_name: AssistantName::new(assistant).expect("valid assistant name"),
        user_name: None,
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

/// Presence on a current session with the onboarding window showing at the welcome, its view
/// in a GPUI test window without the native surface.
fn showing(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    AnyWindowHandle,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let (window, controls) = owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        let controls = owner.link.record_controls();
        owner.settings.update(cx, |state, _| {
            assert_eq!(
                state.set_view(view("Kumo", false), Update::Unsolicited),
                None
            );
        });
        let state = Onboarding::begin(&view("Kumo", false)).expect("onboarding is owed");
        let (model, events, weak) = (
            owner.settings.clone(),
            owner.events.clone(),
            cx.weak_entity(),
        );
        let (window, view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| OnboardingView::new(state, model, weak, events, window, cx))
        })
        .expect("a GPUI test window opens");
        owner.onboarding = Some(OnboardingPanel {
            window,
            view,
            bounds: [0; 4],
            previous: None,
        });
        (window, controls)
    });
    focus_input(&owner, window, cx);
    (owner, window, controls)
}

fn focus_input(owner: &Entity<Presence>, window: AnyWindowHandle, cx: &mut TestAppContext) {
    let view = owner.read_with(cx, |owner, _| {
        owner.onboarding.as_ref().expect("showing").view.clone()
    });
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        view.update(cx, |view, cx| view.focus_input(Select::Keep, window, cx));
    });
    frame.run_until_parked();
}

fn step(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Step {
    owner.read_with(cx, |owner, cx| {
        owner
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .read(cx)
            .state
            .step()
    })
}

/// Esc in the window, then the event it sent, applied as the presence loop applies it.
fn escape(owner: &Entity<Presence>, window: AnyWindowHandle, cx: &mut TestAppContext) {
    VisualTestContext::from_window(window, cx).simulate_keystrokes("escape");
    owner.update(cx, |owner, cx| {
        let event = owner
            .events_in
            .try_recv()
            .expect("the window's Escape action");
        assert!(matches!(event, PresenceEvent::Escape));
        owner.on_event(event, cx).expect("Esc");
    });
    cx.run_until_parked();
}

fn rename(owner: &Entity<Presence>, name: &str, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, cx| {
            state.change(Field::Assistant, name);
            cx.notify();
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
fn enter_with_the_saved_name_moves_focus_to_the_next_steps_input(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(step(&owner, cx), Step::YourName);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        let view = owner
            .read(cx)
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .clone();
        let input = view.read(cx).inputs[1].read(cx).focus_handle(cx);
        assert!(input.is_focused(window), "focus is in the name input");
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn esc_restores_an_edited_name_then_a_second_esc_puts_onboarding_off(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    rename(&owner, "Mochi", cx);
    escape(&owner, window, cx);
    owner.read_with(cx, |owner, cx| {
        assert!(owner.onboarding.is_some(), "the window stays");
        assert_eq!(owner.settings.read(cx).value(Field::Assistant), "Kumo");
    });
    escape(&owner, window, cx);
    owner.read_with(cx, |owner, _| {
        assert!(owner.onboarding.is_none());
        assert!(owner.onboarding_later);
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn finish_later_saves_a_valid_edited_name_and_sends_no_finish(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    rename(&owner, "Mochi", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let close = frame
        .debug_bounds("onboarding-close")
        .expect("the Finish later button");
    frame.simulate_click(close.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.onboarding.is_none());
        assert!(owner.onboarding_later);
    });
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::SetAssistantName(
            AssistantName::new("Mochi").expect("valid")
        )))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn an_onboarded_view_closes_the_window_quietly(cx: &mut TestAppContext) {
    let (owner, _, _) = showing(cx);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Settings {
                    generation: 7,
                    view: view("Kumo", true),
                    answer: false,
                }]),
                cx,
            )
            .expect("an unsolicited view");
        assert!(owner.onboarding.is_none());
        assert!(!owner.onboarding_later);
        assert_eq!(owner.panel_visibility, PanelVisibility::Hidden);
    });
}

#[gpui::test]
fn the_finish_lands_on_the_peek_only_when_the_brain_says_so(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::FinishOnboarding))
    );
    owner.read_with(cx, |owner, _| assert!(owner.onboarding.is_some()));
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Settings {
                    generation: 7,
                    view: view("Kumo", true),
                    answer: true,
                }]),
                cx,
            )
            .expect("the finish's answer");
        assert!(owner.onboarding.is_none());
        assert_eq!(owner.panel_visibility, PanelVisibility::Visible);
    });
}

/// A double tap's effects, from timed key events through the recogniser.
fn double_tap() -> [Option<Effect>; 3] {
    let start = Instant::now();
    let mut gesture = Gesture::legacy_hold_fixture();
    let mut effects = [None; 3];
    for (key, edge, at) in [
        (Key::LeftControl, Edge::Down, 0),
        (Key::LeftAlt, Edge::Down, 0),
        (Key::LeftControl, Edge::Up, 0),
        (Key::LeftAlt, Edge::Up, 0),
        (Key::LeftControl, Edge::Down, 100),
        (Key::LeftAlt, Edge::Down, 100),
        (Key::LeftAlt, Edge::Up, 200),
    ] {
        effects = gesture.handle(Event::Key { key, edge }, start + Duration::from_millis(at));
    }
    assert_eq!(
        effects,
        [Some(Effect::Resting), Some(Effect::OpenInput), None]
    );
    effects
}

fn guarded(owner: &Entity<Presence>, cx: &mut TestAppContext) -> [bool; 2] {
    let dock = owner.read_with(cx, |owner, _| owner.dock_window);
    cx.update(|cx| {
        dock.update(cx, |_, window, cx| {
            owner.update(cx, |owner, cx| {
                owner.apply_effects(
                    double_tap(),
                    EventTime {
                        instant: Instant::now(),
                    },
                    window,
                    cx,
                );
                let typing = owner.text_open();
                owner.dismiss_text(cx).expect("hide typing");
                for event in [
                    PresenceEvent::ShowStatus,
                    PresenceEvent::DockPressed { qpc: 0 },
                ] {
                    owner.on_event(event, cx).expect("the dock's events");
                }
                [typing, owner.panel_visibility == PanelVisibility::Visible]
            })
        })
        .expect("the dock window")
    })
}

#[gpui::test]
fn the_double_tap_and_the_taskbar_button_leave_onboarding_on_top(cx: &mut TestAppContext) {
    let (owner, _, _) = showing(cx);
    assert_eq!(guarded(&owner, cx), [false, false]);
    owner.update(cx, |owner, cx| owner.close_onboarding(cx).expect("close"));
    assert_eq!(guarded(&owner, cx), [true, true]);
}
