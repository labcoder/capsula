use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
    WindowOptions,
};
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, DockPosition, GestureId, Incarnation, MaxFrameRate, NameError,
    OnboardingStep, SettingsOp, SettingsView, SttUnload, ThreadId, UserName,
};

use super::{OnboardingPanel, OnboardingView, Select};
use crate::input::{Edge, Effect, Event, Gesture, Key};
use crate::link::{Batch, LinkEvent, Session};
use crate::onboarding::{Onboarding, Step, ThisRun};
use crate::platform::{Deliver, EventTime, PresenceEvent};
use crate::presence::settings::field_status;
use crate::presence::voice_wiring_tests::ready;
use crate::presence::{PanelVisibility, Presence, test_fixture};
use crate::settings::{Error, Field, Update};
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
    let window = open_over(&owner, cx);
    focus_input(&owner, window, cx);
    (owner, window, controls)
}

/// Shows the onboarding window over `owner` through the setter `open_onboarding` uses.
fn open_over(owner: &Entity<Presence>, cx: &mut TestAppContext) -> AnyWindowHandle {
    owner.update(cx, |owner, cx| {
        let state =
            Onboarding::begin(&view("Kumo", false), ThisRun::Owed).expect("onboarding is owed");
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
            field_status(settings, Field::Assistant, strings),
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
    let session = owner.update(cx, |owner, _| owner.session.take());
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    let unsent = |owner: &Entity<Presence>, cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, cx| {
            owner
                .onboarding
                .as_ref()
                .expect("showing")
                .view
                .read(cx)
                .unsent
        })
    };
    assert!(unsent(&owner, cx));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(cx, |owner, _| owner.session = session);
    VisualTestContext::from_window(window, cx).simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!unsent(&owner, cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(
            SettingsOp::CompleteOnboardingStep(OnboardingStep::YourName)
        ))
    );
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
    let sent = std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        sent,
        [OnboardingStep::Welcome, OnboardingStep::YourName]
            .map(|step| ControlToBrain::Settings(SettingsOp::CompleteOnboardingStep(step)))
            .into_iter()
            .chain([ControlToBrain::Settings(SettingsOp::FinishOnboarding)])
            .collect::<Vec<_>>()
    );
    owner.update(cx, |owner, cx| {
        for _ in 0..2 {
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
        }
        assert!(owner.onboarding.is_some());
        assert!(owner.settings.read(cx).finishing(), "still the finish's");
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
            Rc::clone(&owner.gesture),
            owner.dock_window,
            weak,
            owner.link.clone(),
            Rc::clone(&owner.voice.target),
            Rc::clone(&owner.voice.paused),
            cx,
        )
    })
}

fn chord(deliver: &Deliver, edge: Edge, at: Instant) {
    for key in [Key::LeftControl, Key::LeftAlt] {
        deliver(Event::Key { key, edge }, EventTime { instant: at });
    }
}

fn at_ms(at: Instant, millis: u64) -> EventTime {
    EventTime {
        instant: at + Duration::from_millis(millis),
    }
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
    assert_eq!(ended, [cancel.clone(), stopped, cancel]);
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
