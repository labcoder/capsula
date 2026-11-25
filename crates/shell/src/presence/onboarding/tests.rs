use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, Focusable as _, Modifiers, Pixels, Point,
    ScrollDelta, ScrollEvent, Size, TestAppContext, VisualTestContext, WindowOptions, point, px,
    size,
};
use kumochi_protocol::message::{
    AccountEmail, AccountOp, AccountRecovery, AccountView, Action, AssistantName, ClaudeCodeView,
    ClaudeVersion, ConnectionOp, ConnectionsView, ControlToBrain, DockPosition, ErrorKey,
    GestureId, Incarnation, KeyView, MaxFrameRate, NameError, OnboardingStep, PlanState,
    SavedSignInFailure, SettingsOp, SettingsView, SttUnload, ThreadId, UserError, UserName,
};

use super::{OnboardingPanel, OnboardingView, Select};
use crate::input::{Edge, Effect, Event, Gesture, Key};
use crate::link::{Batch, LinkEvent, LinkState, Session};
use crate::onboarding::{Onboarding, Step, ThisRun};
use crate::platform::{Deliver, EventTime, PresenceEvent};
use crate::presence::voice_wiring_tests::ready;
use crate::presence::{PanelVisibility, Presence, test_fixture};
use crate::settings::{Error, Field, Update};
use crate::sign_in_mark::SignInMark;
use crate::strings::{Strings, Text};

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
        screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
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
    let (owner, controls) = connected(cx);
    let window = open_over(&owner, cx);
    focus_input(&owner, window, cx);
    (owner, window, controls)
}

/// Presence on connection 7 with a settings view that owes onboarding, before the window opens.
fn connected(
    cx: &mut TestAppContext,
) -> (Entity<Presence>, async_channel::Receiver<ControlToBrain>) {
    let owner = test_fixture::presence(cx);
    let controls = owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        let controls = owner.link.record_controls();
        owner.settings.update(cx, |state, _| {
            assert_eq!(
                state.set_view(view("Kumo", false), Update::Unsolicited),
                None
            );
        });
        controls
    });
    (owner, controls)
}

/// Shows the onboarding window over `owner` through the setter `open_onboarding` uses.
fn open_over(owner: &Entity<Presence>, cx: &mut TestAppContext) -> AnyWindowHandle {
    open_at(owner, OnboardingStep::Welcome, cx)
}

/// As [`open_over`], resuming at `from` as a launch does.
fn open_at(
    owner: &Entity<Presence>,
    from: OnboardingStep,
    cx: &mut TestAppContext,
) -> AnyWindowHandle {
    owner.update(cx, |owner, cx| {
        let resumed = SettingsView {
            onboarding_from: from,
            ..view("Kumo", false)
        };
        let state = Onboarding::begin(&resumed, ThisRun::Owed).expect("onboarding is owed");
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
    })
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

/// Continue or Not now, as the footer button runs it.
fn press_continue(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner.onboarding_continue(cx).expect("continue");
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
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(
            SettingsOp::CompleteOnboardingStep(OnboardingStep::Welcome)
        ))
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

/// The welcome gets Kumo's still pose once its bake lands: the peek's 64 px at a test window's
/// scale of 2.
#[gpui::test]
fn the_welcome_shows_a_still_kumo_at_the_peeks_size(cx: &mut TestAppContext) {
    let (owner, window, _controls) = showing(cx);
    let scale =
        VisualTestContext::from_window(window, cx).update(|window, _| window.scale_factor());
    assert!((scale - 2.0).abs() < f32::EPSILON, "a test window's scale");
    let image = owner
        .read_with(cx, |owner, cx| {
            owner
                .onboarding
                .as_ref()
                .expect("showing")
                .view
                .read(cx)
                .kumo
                .clone()
        })
        .expect("the bake landed");
    let pixels = gpui::DevicePixels(128);
    assert_eq!(image.size(0), gpui::size(pixels, pixels));
}

fn selected(owner: &Entity<Presence>, cx: &mut TestAppContext) -> std::ops::Range<usize> {
    owner.read_with(cx, |owner, cx| {
        owner
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .read(cx)
            .input()
            .expect("a name step")
            .read(cx)
            .selected_range()
    })
}

#[gpui::test]
fn moving_to_a_step_selects_its_saved_name(cx: &mut TestAppContext) {
    let (owner, window, _) = showing(cx);
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, cx| {
            state.set_view(
                SettingsView {
                    user_name: Some(UserName::new("Oscar").expect("valid user name")),
                    ..view("Kumo", false)
                },
                Update::Unsolicited,
            );
            cx.notify();
        });
    });
    cx.run_until_parked();
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(step(&owner, cx), Step::YourName);
    assert_eq!(selected(&owner, cx), 0..5);
}

#[gpui::test]
fn enter_on_a_name_that_cant_be_saved_stays_with_focus_in_the_input(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    rename(&owner, "", cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(step(&owner, cx), Step::Welcome);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        let settings = owner.read(cx).settings.read(cx);
        assert_eq!(
            settings.field(Field::Assistant).error,
            Some(Error::Validation(NameError::Empty))
        );
        let strings = cx.global::<Strings>();
        assert_eq!(
            settings.field_status(Field::Assistant, strings),
            (None, Some(strings.get(Text::RenameEmpty)))
        );
        let view = owner
            .read(cx)
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .clone();
        let input = view.read(cx).inputs[0].read(cx).focus_handle(cx);
        assert!(input.is_focused(window), "focus stays in the name input");
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn an_unsent_finish_says_so_and_can_be_pressed_again(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(
            SettingsOp::CompleteOnboardingStep(OnboardingStep::Welcome)
        ))
    );
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(
            SettingsOp::CompleteOnboardingStep(OnboardingStep::YourName)
        ))
    );
    assert_eq!(step(&owner, cx), Step::Accounts);
    press_continue(&owner, cx);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(
            SettingsOp::CompleteOnboardingStep(OnboardingStep::Accounts)
        ))
    );
    assert_eq!(step(&owner, cx), Step::Shortcut);
    let session = owner.update(cx, |owner, _| owner.session.take());
    press_continue(&owner, cx);
    let unsent = |owner: &Entity<Presence>, cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, cx| {
            owner
                .onboarding
                .as_ref()
                .expect("showing")
                .view
                .read(cx)
                .state
                .unsent()
        })
    };
    assert!(unsent(&owner, cx));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(cx, |owner, _| owner.session = session);
    press_continue(&owner, cx);
    assert!(!unsent(&owner, cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::FinishOnboarding))
    );
}

#[gpui::test]
fn showing_with_select_all_selects_the_name(cx: &mut TestAppContext) {
    let (owner, _, _) = showing(cx);
    owner.update(cx, |owner, cx| {
        owner.show_onboarding(Select::All, cx).expect("show");
    });
    cx.run_until_parked();
    assert_eq!(selected(&owner, cx), 0..4);
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
        assert_eq!(owner.onboarding_run, ThisRun::PutOff);
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
        assert_eq!(owner.onboarding_run, ThisRun::PutOff);
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
fn finish_later_drops_a_name_that_cant_be_saved(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    rename(&owner, "", cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let close = frame
        .debug_bounds("onboarding-close")
        .expect("the Finish later button");
    frame.simulate_click(close.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    owner.read_with(cx, |owner, cx| {
        assert!(owner.onboarding.is_none());
        let settings = owner.settings.read(cx);
        assert_eq!(settings.value(Field::Assistant), "Kumo");
        assert_eq!(settings.field(Field::Assistant).error, None);
        assert_eq!(settings.dock_notice(), None);
    });
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
        assert_eq!(owner.onboarding_run, ThisRun::Owed);
        assert_eq!(owner.panel_visibility, PanelVisibility::Hidden);
    });
}

#[gpui::test]
fn an_onboarded_view_drops_an_unsaved_edit_as_it_closes(cx: &mut TestAppContext) {
    let (owner, _, controls) = showing(cx);
    rename(&owner, "Mochi", cx);
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
        assert_eq!(owner.settings.read(cx).value(Field::Assistant), "Kumo");
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn after_finish_later_a_view_that_owes_onboarding_opens_nothing_this_run(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    open_over(&owner, cx);
    owner.update(cx, |owner, cx| {
        owner.dismiss_onboarding(cx).expect("Finish later");
    });
    let (_, sent) = recognised(&deliver, &controls, cx);
    assert_eq!(sent.len(), 2, "the hold is recognised");
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Settings {
                    generation: 1,
                    view: view("Kumo", false),
                    answer: false,
                }]),
                cx,
            )
            .expect("a view that owes onboarding");
        assert!(owner.onboarding.is_none());
    });
    cx.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Err(async_channel::TryRecvError::Empty),
        "an opening window would have cancelled the hold"
    );
}

#[gpui::test]
fn the_finish_lands_on_the_peek_only_when_the_brain_says_so(cx: &mut TestAppContext) {
    let (owner, window, controls) = showing(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    press_continue(&owner, cx);
    press_continue(&owner, cx);
    let sent = std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        sent,
        [
            OnboardingStep::Welcome,
            OnboardingStep::YourName,
            OnboardingStep::Accounts
        ]
        .map(|step| ControlToBrain::Settings(SettingsOp::CompleteOnboardingStep(step)))
        .into_iter()
        .chain([ControlToBrain::Settings(SettingsOp::FinishOnboarding)])
        .collect::<Vec<_>>()
    );
    owner.update(cx, |owner, cx| {
        for _ in 0..3 {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Settings {
                        generation: 7,
                        view: view("Kumo", false),
                        answer: true,
                    }]),
                    cx,
                )
                .expect("a step's answer");
            assert!(owner.onboarding.is_some());
            assert_ne!(owner.panel_visibility, PanelVisibility::Visible);
        }
    });
    press_continue(&owner, cx);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
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

#[gpui::test]
fn a_failed_open_leaves_nothing_open_and_isnt_retried_this_run(cx: &mut TestAppContext) {
    // GPUI's test windows have no native handle, so configuring the surface fails here.
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Settings {
                    generation: 7,
                    view: view("Kumo", false),
                    answer: false,
                }]),
                cx,
            )
            .expect("a view that owes onboarding");
        assert!(owner.onboarding.is_none());
        assert_eq!(owner.onboarding_run, ThisRun::PutOff);
    });
}

/// A double tap's effects, from timed key events through the recogniser.
fn double_tap() -> [Option<Effect>; 3] {
    let start = Instant::now();
    let mut gesture = Gesture::default();
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
                owner.apply_effects(double_tap(), window, cx);
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
fn the_double_tap_and_the_taskbar_button_open_nothing_over_onboarding(cx: &mut TestAppContext) {
    let (owner, _, _) = showing(cx);
    assert_eq!(guarded(&owner, cx), [false, false]);
    owner.update(cx, |owner, cx| owner.close_onboarding(cx).expect("close"));
    assert_eq!(guarded(&owner, cx), [true, true]);
}

fn native(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Deliver {
    let weak = owner.downgrade();
    owner.update(cx, |owner, cx| {
        crate::presence::gesture::deliver(
            (owner.dock_window, weak),
            owner.link.clone(),
            Rc::clone(&owner.voice.target),
            Rc::clone(&owner.voice.paused),
            owner.voice.gates.clone(),
            (Rc::clone(&owner.speech), Rc::clone(&owner.escape_scope)),
            cx,
        )
    })
}

fn chord(deliver: &Deliver, edge: Edge, at: Instant) {
    for key in [Key::LeftControl, Key::LeftAlt] {
        deliver(Event::Key { key, edge }, EventTime::new(at));
    }
}

fn at_ms(at: Instant, millis: u64) -> EventTime {
    EventTime::new(at + Duration::from_millis(millis))
}

/// A whole hold: the chord, the 300 ms threshold through `threshold` (GPUI's scheduled timer
/// when `None`), the release at 400 ms, then two quiet seconds.
fn whole_hold(deliver: &Deliver, threshold: Option<Event>, cx: &mut TestAppContext) {
    let at = cx.background_executor.now();
    chord(deliver, Edge::Down, at);
    cx.run_until_parked();
    if let Some(event) = threshold {
        deliver(event, at_ms(at, 300));
    } else {
        cx.background_executor
            .advance_clock(Duration::from_millis(300));
    }
    cx.run_until_parked();
    chord(deliver, Edge::Up, at + Duration::from_millis(400));
    cx.run_until_parked();
    cx.background_executor.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
}

/// The chord held past the native deadline, then what it sent the brain.
fn recognised(
    deliver: &Deliver,
    controls: &async_channel::Receiver<ControlToBrain>,
    cx: &mut TestAppContext,
) -> (Instant, Vec<ControlToBrain>) {
    let at = cx.background_executor.now();
    chord(deliver, Edge::Down, at);
    deliver(Event::Deadline, at_ms(at, 300));
    cx.run_until_parked();
    (
        at,
        std::iter::from_fn(|| controls.try_recv().ok()).collect(),
    )
}

#[gpui::test]
fn holds_while_onboarding_shows_reach_neither_the_brain_nor_the_microphone(
    cx: &mut TestAppContext,
) {
    test_fixture::without_desktop(cx);
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    open_over(&owner, cx);
    for threshold in [
        Some(Event::Deadline),
        None,
        Some(Event::Key {
            key: Key::LeftAlt,
            edge: Edge::Down,
        }),
        Some(Event::Key {
            key: Key::LeftAlt,
            edge: Edge::Up,
        }),
    ] {
        whole_hold(&deliver, threshold, cx);
        assert!(controls.is_empty(), "{threshold:?}");
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.link.microphone_gate().latest(), 0, "{threshold:?}");
            assert!(owner.voice.pending.is_none(), "{threshold:?}");
            assert_eq!(owner.voice.status, None, "{threshold:?}");
        });
    }
    owner.update(cx, |owner, cx| owner.close_onboarding(cx).expect("close"));
    let (_, sent) = recognised(&deliver, &controls, cx);
    assert!(matches!(
        sent[..],
        [
            ControlToBrain::KeyDown {
                gesture: GestureId(5),
                ..
            },
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(5),
                ..
            },
        ]
    ));
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
    });
}

#[gpui::test]
fn a_hold_held_as_onboarding_opens_ends_as_refused_and_the_next_one_after_close_works(
    cx: &mut TestAppContext,
) {
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    let (at, sent) = recognised(&deliver, &controls, cx);
    assert!(matches!(
        sent[..],
        [
            ControlToBrain::KeyDown {
                gesture: GestureId(1),
                ..
            },
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                ..
            },
        ]
    ));
    open_over(&owner, cx);
    cx.run_until_parked();
    let ended = std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>();
    let cancel = ControlToBrain::GestureCancel {
        gesture: GestureId(1),
    };
    let stopped = ControlToBrain::VoiceHoldStopped {
        gesture: GestureId(1),
    };
    assert_eq!(ended, [cancel, stopped]);
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
        assert!(!owner.link.input_is(GestureId(1)));
    });
    chord(&deliver, Edge::Up, at + Duration::from_millis(400));
    cx.run_until_parked();
    assert!(controls.is_empty());
    owner.update(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
    });
    owner.update(cx, |owner, cx| owner.close_onboarding(cx).expect("close"));
    cx.background_executor.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let (_, sent) = recognised(&deliver, &controls, cx);
    assert!(matches!(
        sent[..],
        [
            ControlToBrain::KeyDown {
                gesture: GestureId(2),
                ..
            },
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(2),
                ..
            },
        ]
    ));
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
    });
}

fn account(owner: &Entity<Presence>, generation: u64, view: AccountView, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Account { generation, view }]),
                cx,
            )
            .expect("an account view");
    });
    cx.run_until_parked();
}

fn claude(
    owner: &Entity<Presence>,
    generation: u64,
    view: ClaudeCodeView,
    cx: &mut TestAppContext,
) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Connections {
                    generation,
                    view: ConnectionsView {
                        anthropic: KeyView::Missing,
                        anthropic_revision: None,
                        claude_code: view,
                    },
                }]),
                cx,
            )
            .expect("a connections view");
    });
    cx.run_until_parked();
}

const SIGNED_OUT: AccountView = AccountView::SignedOut {
    revocation_unconfirmed: false,
};

const CLAUDE_OUT: ClaudeCodeView = ClaudeCodeView::SignedOut {
    version: ClaudeVersion {
        major: 2,
        minor: 1,
        patch: 306,
    },
};

fn drain(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| controls.try_recv().ok()).collect()
}

#[gpui::test]
fn saved_sign_in_dispatch_from_settings_preserves_hold(cx: &mut TestAppContext) {
    saved_sign_in_dispatch(cx, Presence::settings_account_op);
}

#[gpui::test]
fn saved_sign_in_dispatch_from_onboarding_preserves_hold(cx: &mut TestAppContext) {
    saved_sign_in_dispatch(cx, Presence::onboarding_account_op);
}

fn saved_sign_in_dispatch(
    cx: &mut TestAppContext,
    dispatch: impl Fn(&mut Presence, AccountOp, &mut gpui_kit::Context<'_, Presence>),
) {
    const CHECKABLE: AccountView = AccountView::SavedSignInUnavailable {
        reason: SavedSignInFailure::Missing,
        checkable: true,
        checking: false,
        revocation_unconfirmed: false,
    };
    test_fixture::without_desktop(cx);
    let (owner, _data, _controls) = ready(cx);
    let controls = owner.update(cx, |owner, _| owner.link.record_controls_bounded());
    let (_, stamp) = crate::presence::voice_wiring_tests::hold(&owner, 1, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: Some(ThreadId(1)),
            selection: 1,
        }]
    );
    account(&owner, 1, CHECKABLE, cx);
    let owed = SignInMark::CancelOwed { generation: 1 };
    owner.update(cx, |owner, _| {
        owner.settings_chatgpt = owed;
        owner.onboarding_chatgpt = owed;
    });
    let check = AccountOp::CheckSavedSignIn;
    let send = |op, cx: &mut TestAppContext| owner.update(cx, |owner, cx| dispatch(owner, op, cx));
    let assert_preserved = |cx: &mut TestAppContext, waiting, send_failed| {
        owner.read_with(cx, |owner, cx| {
            let accounts = &owner.settings.read(cx).accounts;
            assert_eq!(
                (accounts.waiting, accounts.send_failed),
                (waiting, send_failed)
            );
            assert_eq!(
                [owner.settings_chatgpt, owner.onboarding_chatgpt],
                [owed; 2]
            );
            assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
            assert!(owner.voice.pending.is_some());
            assert!(owner.link.input_is(GestureId(1)));
            assert_eq!(owner.link.choice_revision(), stamp.choice);
            assert!(owner.session.as_ref().unwrap().open());
        });
    };

    send(check, cx);
    assert_preserved(cx, Some(check), false);
    assert_eq!(drain(&controls), [ControlToBrain::AccountOp(check)]);

    send(check, cx);
    assert_preserved(cx, Some(check), false);
    assert_eq!(
        drain(&controls),
        [],
        "a stale callback while the check is pending"
    );

    account(&owner, 1, CHECKABLE, cx);
    let capacity = controls.capacity().unwrap();
    owner.update(cx, |owner, _| {
        let session = owner.session.as_ref().unwrap();
        for _ in 0..capacity {
            assert!(owner.link.send_control(session, ControlToBrain::Pong));
        }
    });
    send(check, cx);
    assert_preserved(cx, None, true);
    assert_eq!(drain(&controls), vec![ControlToBrain::Pong; capacity]);

    account(
        &owner,
        1,
        AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: Some(AccountRecovery::LoadModels),
        },
        cx,
    );
    send(AccountOp::LoadModels, cx);
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.settings.read(cx).accounts.waiting,
            Some(AccountOp::LoadModels)
        );
        assert_eq!(
            [owner.settings_chatgpt, owner.onboarding_chatgpt],
            [SignInMark::Settled; 2]
        );
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
        assert!(owner.voice.pending.is_none());
        assert!(!owner.link.input_is(GestureId(1)));
        assert_eq!(owner.link.choice_revision(), stamp.choice + 1);
    });
}

/// The window at the account step on connection 7, signed out of both connections, with the two
/// step records drained.
fn on_accounts(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    AnyWindowHandle,
    async_channel::Receiver<ControlToBrain>,
) {
    let (owner, window, controls) = showing(cx);
    account(&owner, 7, SIGNED_OUT, cx);
    claude(&owner, 7, CLAUDE_OUT, cx);
    for _ in 0..2 {
        VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
        cx.run_until_parked();
    }
    assert_eq!(step(&owner, cx), Step::Accounts);
    assert_eq!(drain(&controls).len(), 2, "the two step records");
    (owner, window, controls)
}

/// Continue with `ChatGPT` from the step, as its button does.
fn sign_in(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner.onboarding_account_op(AccountOp::SignIn, cx);
    });
}

fn chatgpt_mark(owner: &Entity<Presence>, cx: &mut TestAppContext) -> SignInMark {
    owner.read_with(cx, |owner, _| owner.onboarding_chatgpt)
}

/// Connect… with Claude Code from the step, as its button does.
fn connect(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .settings
            .update(cx, |state, _| state.claude.open_connect());
        owner.onboarding_claude_op(ConnectionOp::ConnectClaudeCode, cx);
    });
}

fn marks(owner: &Entity<Presence>, cx: &mut TestAppContext) -> [SignInMark; 2] {
    owner.read_with(cx, |owner, _| {
        [owner.onboarding_chatgpt, owner.onboarding_claude]
    })
}

fn link(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(vec![event]), cx)
            .expect("a link event");
    });
    cx.run_until_parked();
}

fn error(owner: &Entity<Presence>, key: ErrorKey, action: Action, cx: &mut TestAppContext) {
    let error = UserError::new(key, Vec::new(), vec![action]).expect("one action");
    link(
        owner,
        LinkEvent::Error {
            generation: 7,
            error,
        },
        cx,
    );
}

/// The sign-in ops sent, without what a new connection requests.
fn sign_in_ops(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    drain(controls)
        .into_iter()
        .filter(|control| {
            matches!(
                control,
                ControlToBrain::AccountOp(_) | ControlToBrain::Connection(_)
            )
        })
        .collect()
}

const CANCELS: [ControlToBrain; 2] = [
    ControlToBrain::AccountOp(AccountOp::CancelSignIn),
    ControlToBrain::Connection(ConnectionOp::CancelClaudeSignIn),
];

const COMPLETE: AccountView = AccountView::SignedIn {
    email: None,
    plan: PlanState::Enabled,
    recovery: None,
};

fn open_account_step(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    AnyWindowHandle,
    async_channel::Receiver<ControlToBrain>,
) {
    let (owner, controls) = connected(cx);
    account(&owner, 7, SIGNED_OUT, cx);
    claude(&owner, 7, CLAUDE_OUT, cx);
    let window = open_at(&owner, OnboardingStep::Accounts, cx);
    focus_input(&owner, window, cx);
    (owner, window, controls)
}

fn draw_account_step(frame: &mut VisualTestContext, smaller_viewport: Option<Size<Pixels>>) {
    frame.simulate_scale_factor_change(1.0);
    let viewport = smaller_viewport.unwrap_or_else(|| {
        let rem = frame.update(|window, _| window.rem_size());
        size(rem * super::SIZE_REMS[0], rem * super::SIZE_REMS[1])
    });
    frame.simulate_resize(viewport);
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
}

fn visible_account_control(frame: &mut VisualTestContext, selector: &'static str) -> Point<Pixels> {
    let viewport = frame
        .debug_bounds("onboarding-accounts")
        .expect("account viewport");
    let bounds = frame.debug_bounds(selector).expect(selector);
    assert_eq!(
        bounds.intersect(&viewport),
        bounds,
        "{selector} is clipped by the account viewport"
    );
    bounds.center()
}

#[gpui::test]
fn signed_out_onboarding_can_start_chatgpt_sign_in_without_scrolling(cx: &mut TestAppContext) {
    for viewport in [None, Some(size(px(470.0), px(343.0)))] {
        let (_owner, window, controls) = open_account_step(cx);
        let mut frame = VisualTestContext::from_window(window, cx);
        draw_account_step(&mut frame, viewport);
        let sign_in = visible_account_control(&mut frame, "account-sign-in");
        for selector in [
            "account-provider",
            "account-status",
            "claude-provider",
            "claude-status",
            "claude-review",
        ] {
            visible_account_control(&mut frame, selector);
        }
        frame.simulate_click(sign_in, Modifiers::default());
        frame.run_until_parked();
        assert_eq!(
            drain(&controls),
            [ControlToBrain::AccountOp(AccountOp::SignIn)]
        );
    }
}

#[gpui::test]
fn other_chatgpt_sign_in_actions_also_work_before_scrolling(cx: &mut TestAppContext) {
    for (view, failure, selector, op) in [
        (
            AccountView::SignedOut {
                revocation_unconfirmed: true,
            },
            None,
            "account-sign-in",
            AccountOp::SignIn,
        ),
        (
            AccountView::SigningIn,
            None,
            "account-open-again",
            AccountOp::SignIn,
        ),
        (
            AccountView::SignedIn {
                email: Some(
                    AccountEmail::new("an-extremely-long-test-account-address@example.test")
                        .unwrap(),
                ),
                plan: PlanState::Enabled,
                recovery: Some(AccountRecovery::SignInAgain),
            },
            None,
            "account-sign-in-again",
            AccountOp::SignIn,
        ),
        (
            COMPLETE,
            Some(ErrorKey::SessionEnded),
            "account-sign-in-again",
            AccountOp::SignIn,
        ),
        (
            AccountView::SignedIn {
                email: None,
                plan: PlanState::Declined,
                recovery: None,
            },
            None,
            "account-enable",
            AccountOp::EnablePlan,
        ),
    ] {
        let (owner, window, controls) = open_account_step(cx);
        account(&owner, 7, view, cx);
        if let Some(key) = failure {
            error(&owner, key, Action::SignIn, cx);
        }
        let mut frame = VisualTestContext::from_window(window, cx);
        draw_account_step(&mut frame, None);
        let action = visible_account_control(&mut frame, selector);
        frame.simulate_click(action, Modifiers::default());
        frame.run_until_parked();
        assert_eq!(
            drain(&controls),
            [ControlToBrain::AccountOp(op)],
            "{selector}"
        );
    }
}

#[gpui::test]
fn claude_code_review_keeps_its_disclosures_before_connect(cx: &mut TestAppContext) {
    let (owner, window, controls) = open_account_step(cx);
    let mut frame = VisualTestContext::from_window(window, cx);
    draw_account_step(&mut frame, None);
    let footer = frame.debug_bounds("onboarding-continue").expect("footer");
    let viewport = frame
        .debug_bounds("onboarding-accounts")
        .expect("account viewport");
    let review = visible_account_control(&mut frame, "claude-review");
    frame.simulate_click(review, Modifiers::default());
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        drain(&controls),
        [],
        "review alone does not start a sign-in"
    );
    assert!(frame.debug_bounds("claude-connect").unwrap().top() > viewport.bottom());
    frame.simulate_event(ScrollEvent {
        position: viewport.center(),
        delta: ScrollDelta::Pixels(point(px(0.0), px(-1000.0))),
        ..Default::default()
    });
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let connect = visible_account_control(&mut frame, "claude-connect");
    frame.simulate_click(connect, Modifiers::default());
    frame.run_until_parked();
    assert_eq!(frame.debug_bounds("onboarding-continue"), Some(footer));
    assert_eq!(
        drain(&controls),
        [ControlToBrain::Connection(ConnectionOp::ConnectClaudeCode)]
    );

    account(&owner, 7, COMPLETE, cx);
    claude(
        &owner,
        7,
        ClaudeCodeView::Ready {
            version: ClaudeVersion {
                major: 2,
                minor: 1,
                patch: 306,
            },
            analytics_on: false,
        },
        cx,
    );
    let mut frame = VisualTestContext::from_window(window, cx);
    draw_account_step(&mut frame, None);
    assert_eq!(frame.debug_bounds("account-sign-out"), None);
    assert_eq!(frame.debug_bounds("claude-disconnect"), None);
}

#[gpui::test]
fn enter_on_the_account_step_starts_nothing(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(drain(&controls), []);
    assert_eq!(step(&owner, cx), Step::Accounts);
}

/// The brain may start a sign-in late, so a dismissal cancels one it hasn't shown yet, past the
/// model's gate, once.
#[gpui::test]
fn a_dismissal_before_the_wait_shows_cancels_the_sign_in_once(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    sign_in(&owner, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignIn)]
    );
    escape(&owner, window, cx);
    assert!(owner.read_with(cx, |owner, _| owner.onboarding.is_none()));
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
    account(&owner, 7, AccountView::SigningIn, cx);
    assert_eq!(drain(&controls), [], "a sent cancel isn't owed again");
    assert_eq!(chatgpt_mark(&owner, cx), SignInMark::Settled);
}

#[gpui::test]
fn a_dismissal_while_the_browser_waits_cancels_it(cx: &mut TestAppContext) {
    let (owner, _, controls) = on_accounts(cx);
    sign_in(&owner, cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    drain(&controls);
    owner.update(cx, |owner, cx| {
        owner.dismiss_onboarding(cx).expect("Finish later");
    });
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

/// A sign-in that ended, by leaving the wait or by an error offering a new one, isn't cancelled.
#[gpui::test]
fn a_sign_in_that_ended_is_not_cancelled(cx: &mut TestAppContext) {
    for ended in [false, true] {
        let (owner, window, controls) = on_accounts(cx);
        sign_in(&owner, cx);
        account(&owner, 7, AccountView::SigningIn, cx);
        if ended {
            account(&owner, 7, SIGNED_OUT, cx);
        } else {
            let error = UserError::new(ErrorKey::SignInTimedOut, Vec::new(), vec![Action::SignIn])
                .expect("one action");
            owner.update(cx, |owner, cx| {
                owner
                    .link_events(
                        Batch::Plain(vec![LinkEvent::Error {
                            generation: 7,
                            error,
                        }]),
                        cx,
                    )
                    .expect("the sign-in's error");
            });
        }
        drain(&controls);
        escape(&owner, window, cx);
        assert_eq!(drain(&controls), [], "ended by a view: {ended}");
    }
}

/// Continue and Not now hand waiting sign-ins over for the rest of the window's life: the
/// shortcut step neither cancels them nor adopts them again after a link drop.
#[gpui::test]
fn continue_hands_waiting_sign_ins_over_for_good(cx: &mut TestAppContext) {
    for dropped in [false, true] {
        let (owner, window, controls) = on_accounts(cx);
        sign_in(&owner, cx);
        connect(&owner, cx);
        account(&owner, 7, AccountView::SigningIn, cx);
        claude(&owner, 7, ClaudeCodeView::SigningIn, cx);
        drain(&controls);
        press_continue(&owner, cx);
        assert_eq!(
            drain(&controls),
            [ControlToBrain::Settings(
                SettingsOp::CompleteOnboardingStep(OnboardingStep::Accounts)
            )]
        );
        assert_eq!(step(&owner, cx), Step::Shortcut);
        assert_eq!(marks(&owner, cx), [SignInMark::Settled; 2]);
        if dropped {
            link(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
            link(
                &owner,
                LinkEvent::Connected(Session::detached(8, Incarnation(2))),
                cx,
            );
            account(&owner, 8, AccountView::SigningIn, cx);
            claude(&owner, 8, ClaudeCodeView::SigningIn, cx);
            assert_eq!(marks(&owner, cx), [SignInMark::Settled; 2]);
        }
        escape(&owner, window, cx);
        assert_eq!(sign_in_ops(&controls), [], "dropped: {dropped}");
    }
}

/// The window at the shortcut on connection 7, opened there as a launch resumes it.
fn at_shortcut(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    AnyWindowHandle,
    async_channel::Receiver<ControlToBrain>,
) {
    let (owner, controls) = connected(cx);
    let window = open_at(&owner, OnboardingStep::Shortcut, cx);
    focus_input(&owner, window, cx);
    (owner, window, controls)
}

/// The recogniser's double-tap, applied as the dock's input applies it.
fn tap(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    let dock = owner.read_with(cx, |owner, _| owner.dock_window);
    cx.update(|cx| {
        dock.update(cx, |_, window, cx| {
            owner.update(cx, |owner, cx| {
                owner.apply_effects(double_tap(), window, cx);
            });
        })
        .expect("the dock window");
    });
    cx.run_until_parked();
}

/// At the shortcut a double-tap asks for the finish once; when the brain says onboarded, the
/// window closes and the typing box opens instead of the peek.
#[gpui::test]
fn a_double_tap_at_the_shortcut_finishes_into_typing(cx: &mut TestAppContext) {
    let (owner, _, controls) = at_shortcut(cx);
    tap(&owner, cx);
    tap(&owner, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::Settings(SettingsOp::FinishOnboarding)]
    );
    owner.update(cx, |owner, cx| {
        assert!(
            owner.onboarding.is_some(),
            "only the brain's answer closes it"
        );
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
        assert!(owner.text_open());
        assert_ne!(owner.panel_visibility, PanelVisibility::Visible);
    });
}

#[gpui::test]
fn enter_at_the_shortcut_finishes_nothing(cx: &mut TestAppContext) {
    let (owner, window, controls) = at_shortcut(cx);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(drain(&controls), []);
    assert_eq!(step(&owner, cx), Step::Shortcut);
}

/// A window that opens at the shortcut owns no sign-in, so it adopts none it finds waiting.
#[gpui::test]
fn a_window_at_the_shortcut_adopts_nothing(cx: &mut TestAppContext) {
    let (owner, controls) = connected(cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    claude(&owner, 7, ClaudeCodeView::SigningIn, cx);
    let window = open_at(&owner, OnboardingStep::Shortcut, cx);
    focus_input(&owner, window, cx);
    assert_eq!(marks(&owner, cx), [SignInMark::Settled; 2]);
    escape(&owner, window, cx);
    assert_eq!(drain(&controls), []);
}

/// Cancels that can't be sent stay owed across a link drop: sent at the next connection's wait,
/// dropped when that connection shows none.
#[gpui::test]
fn owed_cancels_survive_a_link_drop(cx: &mut TestAppContext) {
    for waiting in [true, false] {
        let (owner, window, controls) = on_accounts(cx);
        sign_in(&owner, cx);
        connect(&owner, cx);
        assert_eq!(drain(&controls).len(), 2, "both sign-ins");
        link(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        escape(&owner, window, cx);
        assert_eq!(drain(&controls), []);
        assert_eq!(
            marks(&owner, cx),
            [SignInMark::CancelOwed { generation: 7 }; 2]
        );
        link(
            &owner,
            LinkEvent::Connected(Session::detached(8, Incarnation(2))),
            cx,
        );
        if waiting {
            account(&owner, 8, AccountView::SigningIn, cx);
            claude(&owner, 8, ClaudeCodeView::SigningIn, cx);
        } else {
            account(&owner, 8, SIGNED_OUT, cx);
            claude(&owner, 8, CLAUDE_OUT, cx);
        }
        assert_eq!(
            sign_in_ops(&controls),
            &CANCELS[..if waiting { 2 } else { 0 }],
            "{waiting}"
        );
        assert_eq!(marks(&owner, cx), [SignInMark::Settled; 2]);
    }
}

#[gpui::test]
fn a_sign_in_from_elsewhere_drops_an_owed_cancel(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    sign_in(&owner, cx);
    link(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    escape(&owner, window, cx);
    drain(&controls);
    owner.update(cx, |owner, cx| {
        owner.account_op(AccountOp::SignIn, cx);
    });
    assert_eq!(chatgpt_mark(&owner, cx), SignInMark::Settled);
}

/// A frame before the wait shows may predate Connect (it can carry only the Anthropic half), so
/// it ends nothing and the dismissal still cancels; the brain ignores a cancel after a refused
/// Connect.
#[gpui::test]
fn claude_codes_sign_in_from_the_step_is_cancelled_on_dismissal(cx: &mut TestAppContext) {
    for answered in [false, true] {
        let (owner, window, controls) = on_accounts(cx);
        connect(&owner, cx);
        assert_eq!(
            drain(&controls),
            [ControlToBrain::Connection(ConnectionOp::ConnectClaudeCode)]
        );
        if answered {
            claude(&owner, 7, CLAUDE_OUT, cx);
        }
        escape(&owner, window, cx);
        assert_eq!(
            drain(&controls),
            [ControlToBrain::Connection(ConnectionOp::CancelClaudeSignIn)],
            "{answered}"
        );
    }
}

/// Sign in again answered by the account still complete (the brain deferred the start) hasn't
/// ended: the plan line stays hidden, and the dismissal cancels it.
#[gpui::test]
fn a_sign_in_answered_by_the_same_complete_account_is_still_the_windows(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    account(&owner, 7, COMPLETE, cx);
    error(&owner, ErrorKey::SessionEnded, Action::SignIn, cx);
    sign_in(&owner, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignIn)]
    );
    account(&owner, 7, COMPLETE, cx);
    assert!(!owner.read_with(cx, |owner, cx| {
        owner
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .read(cx)
            .plan
    }));
    escape(&owner, window, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

/// Open again replaces the waiting attempt without a new view, so the wait still counts as shown:
/// the completed sign-in shows the plan line and leaves nothing to cancel.
#[gpui::test]
fn open_again_keeps_the_wait_it_replaced(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    sign_in(&owner, cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    sign_in(&owner, cx);
    assert_eq!(
        drain(&controls),
        [
            ControlToBrain::AccountOp(AccountOp::SignIn),
            ControlToBrain::AccountOp(AccountOp::SignIn)
        ]
    );
    account(&owner, 7, COMPLETE, cx);
    assert!(owner.read_with(cx, |owner, cx| {
        owner
            .onboarding
            .as_ref()
            .expect("showing")
            .view
            .read(cx)
            .plan
    }));
    escape(&owner, window, cx);
    assert_eq!(drain(&controls), []);
}

/// An error offering only Retry comes from a renewal or a refused start, so it ends no sign-in.
#[gpui::test]
fn an_error_offering_only_retry_ends_no_sign_in(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    sign_in(&owner, cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    error(&owner, ErrorKey::SignInFailed, Action::Retry, cx);
    drain(&controls);
    escape(&owner, window, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

/// Turning the plan on starts a sign-in, so it's the window's like Sign in.
#[gpui::test]
fn a_plan_turned_on_from_the_step_is_cancelled_on_dismissal(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    account(
        &owner,
        7,
        AccountView::SignedIn {
            email: None,
            plan: PlanState::Declined,
            recovery: None,
        },
        cx,
    );
    owner.update(cx, |owner, cx| {
        owner.onboarding_account_op(AccountOp::EnablePlan, cx);
    });
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::EnablePlan)]
    );
    escape(&owner, window, cx);
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

#[gpui::test]
fn a_ready_claude_code_turns_not_now_into_continue(cx: &mut TestAppContext) {
    let (owner, _, _controls) = on_accounts(cx);
    let usable = |owner: &Entity<Presence>, cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, cx| {
            super::accounts::usable(owner.settings.read(cx))
        })
    };
    assert!(!usable(&owner, cx));
    claude(
        &owner,
        7,
        ClaudeCodeView::Ready {
            version: ClaudeVersion {
                major: 2,
                minor: 1,
                patch: 306,
            },
            analytics_on: false,
        },
        cx,
    );
    assert!(usable(&owner, cx));
}

/// After a shell restart the window owns each waiting sign-in it finds, as it opens or on a
/// connection's first view, and one dismissal cancels both.
#[gpui::test]
fn the_window_adopts_waiting_sign_ins_after_a_restart(cx: &mut TestAppContext) {
    for at_open in [true, false] {
        let (owner, controls) = connected(cx);
        let waiting = |owner: &Entity<Presence>, cx: &mut TestAppContext| {
            account(owner, 7, AccountView::SigningIn, cx);
            claude(owner, 7, ClaudeCodeView::SigningIn, cx);
        };
        if at_open {
            waiting(&owner, cx);
        }
        let window = open_over(&owner, cx);
        focus_input(&owner, window, cx);
        if !at_open {
            waiting(&owner, cx);
        }
        escape(&owner, window, cx);
        assert_eq!(drain(&controls), CANCELS, "{at_open}");
    }
}

/// Without the window nothing is adopted, and a wait shown later on a connection isn't the
/// window's: after its first view, something else started it.
#[gpui::test]
fn the_window_adopts_nothing_while_closed_or_after_a_connections_first_view(
    cx: &mut TestAppContext,
) {
    let (closed, _controls) = connected(cx);
    account(&closed, 7, AccountView::SigningIn, cx);
    claude(&closed, 7, ClaudeCodeView::SigningIn, cx);
    assert_eq!(marks(&closed, cx), [SignInMark::Settled; 2]);
    let (owner, window, controls) = on_accounts(cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    claude(&owner, 7, ClaudeCodeView::SigningIn, cx);
    escape(&owner, window, cx);
    assert_eq!(drain(&controls), []);
}

#[gpui::test]
fn the_plan_line_shows_once_this_windows_sign_in_completes(cx: &mut TestAppContext) {
    let (owner, _, _controls) = on_accounts(cx);
    let shows = |owner: &Entity<Presence>, cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, cx| {
            let view = owner.onboarding.as_ref().expect("showing").view.read(cx);
            (view.plan, super::accounts::usable(owner.settings.read(cx)))
        })
    };
    assert_eq!(shows(&owner, cx), (false, false));
    sign_in(&owner, cx);
    account(&owner, 7, AccountView::SigningIn, cx);
    account(&owner, 7, COMPLETE, cx);
    assert_eq!(shows(&owner, cx), (true, true));
}

/// A refused op opens no mark, so a dismissal has nothing to cancel.
#[gpui::test]
fn a_refused_sign_in_opens_no_mark(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    account(
        &owner,
        7,
        AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None,
        },
        cx,
    );
    sign_in(&owner, cx);
    assert_eq!(drain(&controls), []);
    assert_eq!(chatgpt_mark(&owner, cx), SignInMark::Settled);
    escape(&owner, window, cx);
    assert_eq!(drain(&controls), []);
}

/// Tab and Enter on a section button replace it: focus must come back to the window, so Esc
/// still dismisses it.
#[gpui::test]
fn esc_still_dismisses_after_enter_replaces_a_section_button(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    press(window, "tab tab enter", cx);
    cx.run_until_parked();
    assert!(
        owner.read_with(cx, |owner, cx| owner
            .settings
            .read(cx)
            .claude
            .connect_screen),
        "Enter reached Connect…"
    );
    escape(&owner, window, cx);
    assert!(owner.read_with(cx, |owner, _| owner.onboarding.is_none()));
    assert_eq!(drain(&controls), []);
}

#[gpui::test]
fn esc_cancels_a_keyboard_started_sign_in_once_the_browser_waits(cx: &mut TestAppContext) {
    let (owner, window, controls) = on_accounts(cx);
    press(window, "tab enter", cx);
    cx.run_until_parked();
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::SignIn)]
    );
    account(&owner, 7, AccountView::SigningIn, cx);
    escape(&owner, window, cx);
    assert!(owner.read_with(cx, |owner, _| owner.onboarding.is_none()));
    assert_eq!(
        drain(&controls),
        [ControlToBrain::AccountOp(AccountOp::CancelSignIn)]
    );
}

/// Keys after a paint, so the window knows its tab stops.
fn press(window: AnyWindowHandle, keys: &str, cx: &mut TestAppContext) {
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| window.draw(cx).clear(cx));
    for key in keys.split_whitespace() {
        let keystroke = frame.update(|_, cx| {
            gpui_kit::Keystroke::parse_with_os(key, cx.platform_info().os).expect("test key")
        });
        frame.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        frame.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        frame.update(|window, cx| window.draw(cx).clear(cx));
    }
    frame.run_until_parked();
}
