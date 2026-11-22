use gpui_kit::TestAppContext;
use kumochi_protocol::message::{ErrorKey, PartState, SttUnload};

use crate::settings::Update;
use crate::strings::Text;

fn press(frame: &mut gpui_kit::VisualTestContext, keys: &str) {
    let os = frame.update(|_, cx| cx.platform_info().os);
    for key in keys.split_whitespace() {
        let keystroke = gpui_kit::Keystroke::parse_with_os(key, os).unwrap();
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

fn open(
    cx: &mut TestAppContext,
) -> (
    gpui_kit::AnyWindowHandle,
    gpui_kit::Entity<super::super::SettingsView>,
) {
    let (window, view) = super::super::general::tests::open(cx);
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let point = frame
        .debug_bounds("settings-voice-tab")
        .expect("voice tab")
        .center();
    frame.simulate_click(point, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    frame.update(|_, cx| assert_eq!(view.read(cx).tab, 2));
    (window, view)
}

#[gpui::test]
fn speak_answers_aloud_saves_and_restores_the_authoritative_choice(cx: &mut TestAppContext) {
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};

    let (window, view) = open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    let settings = |frame: &mut gpui_kit::VisualTestContext, on, update, refusal| {
        frame.update(|_, cx| {
            let mut saved = view.read(cx).model.read(cx).view().unwrap().clone();
            saved.talk_back = on;
            let mut events = Vec::new();
            if let Some(error) = refusal {
                events.push(LinkEvent::Error {
                    generation: 7,
                    error,
                });
            }
            events.push(LinkEvent::Settings {
                generation: 7,
                view: saved,
                answer: update == Update::Answer,
            });
            owner.update(cx, |owner, cx| {
                owner.link_events(Batch::Plain(events), cx).unwrap();
            });
        });
    };
    let click = |frame: &mut gpui_kit::VisualTestContext| {
        frame.update(|window, cx| window.draw(cx).clear(cx));
        let center = frame
            .debug_bounds("speak-answers-aloud")
            .expect("Speak answers aloud checkbox")
            .center();
        frame.simulate_click(center, gpui_kit::Modifiers::default());
        frame.run_until_parked();
    };

    settings(&mut frame, true, Update::Unsolicited, None);
    click(&mut frame);
    let off = ControlToBrain::Settings(SettingsOp::SetTalkBack(false));
    assert_eq!(controls.try_recv(), Ok(off.clone()));
    click(&mut frame);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    settings(
        &mut frame,
        true,
        Update::Answer,
        Some(ErrorKey::SettingsRefused.into()),
    );
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("talk-back-error").is_some());
    click(&mut frame);
    assert_eq!(controls.try_recv(), Ok(off));
    press(&mut frame, "ctrl-tab ctrl-shift-tab");
    settings(&mut frame, false, Update::Answer, None);
    click(&mut frame);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::SetTalkBack(true)))
    );
    assert!(controls.is_empty());
}

#[gpui::test]
fn hold_to_talk_is_keyboard_operable_and_waits_for_the_saved_answer(cx: &mut TestAppContext) {
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};

    let (window, view) = open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        view.update(cx, |view, cx| view.focus(window, cx));
        assert!(
            view.read(cx)
                .model
                .read(cx)
                .hold_to_talk_enabled_for_tests()
        );
        window.draw(cx).clear(cx);
    });
    assert!(frame.debug_bounds("hold-to-talk").is_some());
    press(&mut frame, "tab tab space");
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false))
    );
    frame.update(|_, cx| {
        let state = view.read(cx).model.read(cx);
        assert!(state.hold_to_talk_pending());
        assert!(!state.hold_to_talk_enabled_for_tests());
        assert!(state.view().unwrap().hold_to_talk);
    });
    frame.update(|_, cx| {
        let mut saved = view.read(cx).model.read(cx).view().unwrap().clone();
        saved.hold_to_talk = false;
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Settings {
                        generation: 7,
                        view: saved,
                        answer: true,
                    }]),
                    cx,
                )
                .unwrap();
        });
        assert!(!view.read(cx).model.read(cx).hold_to_talk_pending());
        assert!(
            !view
                .read(cx)
                .model
                .read(cx)
                .hold_to_talk_enabled_for_tests()
        );
    });
    assert!(controls.is_empty());
}

#[gpui::test]
fn clicking_a_voice_radio_uses_the_owner_path_and_explains_a_failed_send(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| owner.settings = view.model.clone());
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let point = frame
        .debug_bounds("speech-idle-15m")
        .expect("15-minute radio")
        .center();
    frame.simulate_click(point, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    frame.update(|window, cx| {
        let state = view.read(cx).model.read(cx);
        assert!(state.stt_unload_send_failed());
        assert!(!state.stt_unload_pending());
        assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
        window.draw(cx).clear(cx);
    });
    assert!(frame.debug_bounds("speech-idle-send-failed").is_some());
}

#[gpui::test]
fn keyboard_voice_choice_sends_the_exact_policy_and_waits_for_its_ordered_answer(
    cx: &mut TestAppContext,
) {
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};

    let (window, view) = super::super::general::tests::open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    let control = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    frame.update(|window, cx| {
        view.update(cx, |view, cx| view.focus(window, cx));
        assert_eq!(view.read(cx).tab, 0);
        window.draw(cx).clear(cx);
    });
    for (keys, expected) in [
        ("ctrl-tab", 1),
        ("ctrl-tab", 2),
        ("ctrl-tab", 3),
        ("ctrl-tab", 0),
        ("ctrl-shift-tab", 3),
        ("ctrl-shift-tab", 2),
    ] {
        press(&mut frame, keys);
        frame.run_until_parked();
        frame.update(|window, cx| {
            assert_eq!(view.read(cx).tab, expected);
            assert!(view.read(cx).focus.is_focused(window));
            window.draw(cx).clear(cx);
        });
    }
    press(&mut frame, "tab tab tab tab space");
    frame.run_until_parked();
    assert!(matches!(
        control.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::SetSttUnload(
            SttUnload::After15Min
        )))
    ));
    assert!(matches!(
        control.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    frame.update(|window, cx| {
        let state = view.read(cx).model.read(cx);
        assert!(state.stt_unload_pending());
        assert_eq!(
            state.view().expect("saved view").stt_unload,
            SttUnload::Kept
        );
        assert_eq!(state.stt_unload_choice(), Some(SttUnload::After15Min));
        assert!(!state.stt_unload_send_failed());
        window.draw(cx).clear(cx);
    });
    assert!(frame.debug_bounds("speech-idle-saving").is_some());
    frame.update(|_, cx| {
        let mut saved = view
            .read(cx)
            .model
            .read(cx)
            .view()
            .expect("saved view")
            .clone();
        saved.stt_unload = SttUnload::After15Min;
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Settings {
                        generation: 7,
                        view: saved,
                        answer: true,
                    }]),
                    cx,
                )
                .expect("ordered current-session settings answer");
        });
    });
    frame.run_until_parked();
    frame.update(|window, cx| {
        let state = view.read(cx).model.read(cx);
        assert!(!state.stt_unload_pending());
        assert_eq!(
            state.view().expect("saved view").stt_unload,
            SttUnload::After15Min
        );
        assert_eq!(state.stt_unload_choice(), Some(SttUnload::After15Min));
        window.draw(cx).clear(cx);
    });
    assert!(frame.debug_bounds("speech-idle-saving").is_none());
    assert!(matches!(
        control.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[gpui::test]
fn keyboard_tab_changes_retain_the_edit_and_escape_still_uses_the_settings_path(
    cx: &mut TestAppContext,
) {
    use crate::presence::PresenceEvent;
    use crate::settings::Field;

    let (window, view) = super::super::general::tests::open(cx);
    let (events, received) = async_channel::unbounded();
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.update(|window, _| window.activate_window());
    frame.run_until_parked();
    frame.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.events = events;
            view.inputs[0].update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    frame.run_until_parked();
    frame.simulate_input("RetainedDraft");
    frame.update(|_, cx| assert_eq!(view.read(cx).editing(), Some(Field::Assistant)));
    press(&mut frame, "escape");
    frame.run_until_parked();
    assert!(matches!(received.try_recv(), Ok(PresenceEvent::Escape)));
    press(&mut frame, "ctrl-tab");
    frame.run_until_parked();
    frame.update(|window, cx| {
        let view = view.read(cx);
        assert_eq!(view.tab, 1);
        assert_eq!(view.editing(), None);
        assert_eq!(view.model.read(cx).value(Field::Assistant), "RetainedDraft");
        assert!(view.focus.is_focused(window));
        window.draw(cx).clear(cx);
    });
    press(&mut frame, "ctrl-shift-tab");
    frame.run_until_parked();
    frame.update(|window, cx| {
        assert_eq!(view.read(cx).tab, 0);
        assert_eq!(
            view.read(cx).inputs[0].read(cx).value().as_ref(),
            "RetainedDraft"
        );
        window.draw(cx).clear(cx);
    });
    press(&mut frame, "escape");
    frame.run_until_parked();
    assert!(matches!(received.try_recv(), Ok(PresenceEvent::Escape)));
    assert!(matches!(
        received.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[gpui::test]
fn pending_refused_and_offline_voice_controls_keep_the_saved_contract(cx: &mut TestAppContext) {
    let (window, view) = open(cx);
    view.update(cx, |view, cx| {
        view.model.update(cx, |state, cx| {
            assert!(state.receive_speech_input(Some(1), 1, PartState::Unloaded));
            assert!(state.send_stt_unload(SttUnload::After15Min, |_| true));
            cx.notify();
        });
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("speech-idle-saving").is_some());
    let point = frame
        .debug_bounds("speech-idle-1h")
        .expect("hour radio")
        .center();
    frame.simulate_click(point, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    frame.update(|_, cx| {
        let model = view.read(cx).model.clone();
        assert_eq!(
            model.read(cx).stt_unload_choice(),
            Some(SttUnload::After15Min)
        );
        assert!(!model.read(cx).stt_unload_send_failed());
        model.update(cx, |state, cx| {
            state.error(ErrorKey::SettingsRefused.into());
            let saved = state.view().expect("saved view").clone();
            state.set_view(saved, Update::Answer);
            assert_eq!(state.stt_unload_choice(), Some(SttUnload::Kept));
            assert_eq!(state.speech_input_status(), Text::VoiceUnloaded);
            cx.notify();
        });
    });
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("speech-idle-refused").is_some());
    assert!(frame.debug_bounds("speech-idle-saving").is_none());
    frame.update(|_, cx| {
        view.read(cx).model.clone().update(cx, |state, cx| {
            state.disconnected();
            cx.notify();
        });
    });
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    let point = frame
        .debug_bounds("speech-idle-15m")
        .expect("disabled radio")
        .center();
    frame.simulate_click(point, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    frame.update(|_, cx| {
        let state = view.read(cx).model.read(cx);
        assert_eq!(state.stt_unload_choice(), None);
        assert_eq!(state.speech_input_status(), Text::VoiceStatusUnknown);
        assert!(!state.stt_unload_send_failed());
    });
}

#[gpui::test]
fn current_voice_link_events_reach_the_model_but_old_or_missing_sessions_do_not(
    cx: &mut TestAppContext,
) {
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::Incarnation;

    let owner = crate::presence::test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(7, Incarnation(1)));
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Voice {
                    generation: 7,
                    speech_input: PartState::Unloaded,
                }]),
                cx,
            )
            .expect("voice event");
        assert_eq!(
            owner.settings.read(cx).speech_input_status(),
            Text::VoiceUnloaded
        );
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Voice {
                    generation: 6,
                    speech_input: PartState::Ready,
                }]),
                cx,
            )
            .expect("stale event");
        assert_eq!(
            owner.settings.read(cx).speech_input_status(),
            Text::VoiceUnloaded
        );
        owner.session = None;
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::Voice {
                    generation: 7,
                    speech_input: PartState::Ready,
                }]),
                cx,
            )
            .expect("ended session");
        assert_eq!(
            owner.settings.read(cx).speech_input_status(),
            Text::VoiceUnloaded
        );
    });
}

fn deliver(
    owner: &gpui_kit::Entity<crate::presence::Presence>,
    events: Vec<crate::link::LinkEvent>,
    cx: &mut TestAppContext,
) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(crate::link::Batch::Plain(events), cx)
            .expect("link events");
    });
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one rendered install/cancel flow keeps ordering and focus assertions together"
)]
fn model_buttons_use_pointer_and_space_but_cancel_acceptance_does_not_mean_cancelled(
    cx: &mut TestAppContext,
) {
    use crate::link::{LinkEvent, LinkState, Session};
    use crate::presence::PresenceEvent;
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    for (last, status, action_present) in [
        (PartState::Ready, Text::VoiceReady, false),
        (PartState::Unloaded, Text::VoiceUnloaded, true),
    ] {
        let (window, view) = open(cx);
        let (events, received) = async_channel::unbounded();
        view.update(cx, |view, _| view.events = events);
        let owner = crate::presence::test_fixture::presence(cx);
        let controls = view.update(cx, |view, cx| {
            view.owner = owner.downgrade();
            owner.update(cx, |owner, _| {
                owner.settings = view.model.clone();
                owner.session = Some(Session::detached(7, Incarnation(1)));
                owner.link.record_controls()
            })
        });
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        let saved = frame.update(|_, cx| {
            view.read(cx)
                .model
                .read(cx)
                .view()
                .expect("settings")
                .clone()
        });
        deliver(
            &owner,
            vec![LinkEvent::Voice {
                generation: 7,
                speech_input: PartState::NotInstalled,
            }],
            cx,
        );
        frame.run_until_parked();
        frame.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        let point = frame
            .debug_bounds("speech-model-action")
            .expect("install")
            .center();
        frame.simulate_click(point, gpui_kit::Modifiers::default());
        frame.run_until_parked();
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::Settings(SettingsOp::InstallVoice(
                SpeechInput
            )))
        );
        frame.update(|window, cx| {
            assert_eq!(
                view.read(cx).model.read(cx).speech_input_status(),
                Text::VoiceNotInstalled
            );
            assert!(view.read(cx).focus.is_focused(window));
            window.draw(cx).clear(cx);
        });
        assert!(frame.debug_bounds("speech-model-pending").is_some());
        frame.simulate_click(point, gpui_kit::Modifiers::default());
        frame.run_until_parked();
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        deliver(
            &owner,
            vec![
                LinkEvent::Settings {
                    generation: 6,
                    view: saved.clone(),
                    answer: true,
                },
                LinkEvent::Error {
                    generation: 6,
                    error: ErrorKey::VoiceUnavailable.into(),
                },
                LinkEvent::Voice {
                    generation: 6,
                    speech_input: PartState::Ready,
                },
                LinkEvent::Settings {
                    generation: 7,
                    view: saved.clone(),
                    answer: false,
                },
            ],
            cx,
        );
        frame.update(|_, cx| {
            let model = view.read(cx).model.read(cx);
            assert!(model.speech_input_pending());
            assert_eq!(model.speech_input_error(), None);
            assert_eq!(model.speech_input_status(), Text::VoiceNotInstalled);
        });
        deliver(
            &owner,
            vec![
                LinkEvent::Settings {
                    generation: 7,
                    view: saved.clone(),
                    answer: true,
                },
                LinkEvent::Voice {
                    generation: 7,
                    speech_input: PartState::Loading,
                },
            ],
            cx,
        );
        frame.run_until_parked();
        frame.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
            window.draw(cx).clear(cx);
        });
        assert!(frame.debug_bounds("speech-model-cancel-help").is_some());
        assert!(frame.debug_bounds("speech-model-pending").is_none());
        press(&mut frame, "tab tab tab space");
        frame.run_until_parked();
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::Settings(SettingsOp::CancelVoiceInstall))
        );
        deliver(
            &owner,
            vec![LinkEvent::Settings {
                generation: 7,
                view: saved,
                answer: true,
            }],
            cx,
        );
        frame.update(|window, cx| {
            let model = view.read(cx).model.read(cx);
            assert!(!model.speech_input_pending());
            assert_eq!(model.speech_input_status(), Text::VoiceLoading);
            assert!(view.read(cx).focus.is_focused(window));
        });
        frame.run_until_parked();
        press(&mut frame, "tab");
        let elsewhere = frame.update(|window, cx| window.focused(cx).expect("close button focus"));
        deliver(
            &owner,
            vec![LinkEvent::Voice {
                generation: 7,
                speech_input: PartState::Loading,
            }],
            cx,
        );
        frame.run_until_parked();
        frame.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                elsewhere.is_focused(window),
                "a Voice update must not move ordinary focus"
            );
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        press(&mut frame, "tab tab tab");
        let unpressed = frame.update(|window, cx| {
            assert!(!view.read(cx).focus.is_focused(window));
            window.focused(cx).expect("unpressed model button focus")
        });
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        deliver(
            &owner,
            vec![LinkEvent::Voice {
                generation: 7,
                speech_input: last,
            }],
            cx,
        );
        frame.run_until_parked();
        frame.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(view.read(cx).model.read(cx).speech_input_status(), status);
            assert_eq!(view.read(cx).focus.is_focused(window), !action_present);
            assert_eq!(unpressed.is_focused(window), action_present);
        });
        assert_eq!(
            frame.debug_bounds("speech-model-action").is_some(),
            action_present
        );
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        press(&mut frame, "ctrl-tab escape");
        frame.run_until_parked();
        frame.update(|window, cx| {
            assert_eq!(view.read(cx).tab, 3);
            assert!(view.read(cx).focus.is_focused(window));
        });
        assert!(matches!(received.try_recv(), Ok(PresenceEvent::Escape)));
        press(&mut frame, "ctrl-shift-tab");
        frame.run_until_parked();
        if action_present {
            press(&mut frame, "tab tab tab");
            frame.update(|window, cx| assert!(!view.read(cx).focus.is_focused(window)));
        }
        deliver(&owner, vec![LinkEvent::State(LinkState::Reconnecting)], cx);
        frame.run_until_parked();
        frame.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(view.read(cx).focus.is_focused(window));
            assert_eq!(
                view.read(cx).model.read(cx).speech_input_status(),
                Text::VoiceStatusUnknown
            );
        });
        press(&mut frame, "ctrl-tab escape");
        frame.run_until_parked();
        frame.update(|window, cx| {
            assert_eq!(view.read(cx).tab, 3);
            assert!(view.read(cx).focus.is_focused(window));
        });
        assert!(matches!(received.try_recv(), Ok(PresenceEvent::Escape)));
        assert_eq!(
            received.try_recv().err(),
            Some(async_channel::TryRecvError::Empty)
        );
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    }
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one rendered session fixture verifies stale intents and transport failure before retry"
)]
fn reload_enter_refusal_and_captured_intents_stay_fenced_before_the_recording_seam(
    cx: &mut TestAppContext,
) {
    use crate::link::{LinkEvent, LinkState, Session};
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    let (window, view) = open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    let saved = frame.update(|_, cx| {
        view.read(cx)
            .model
            .read(cx)
            .view()
            .expect("settings")
            .clone()
    });
    deliver(
        &owner,
        vec![LinkEvent::Voice {
            generation: 7,
            speech_input: PartState::Unloaded,
        }],
        cx,
    );
    frame.run_until_parked();
    frame.update(|window, cx| {
        view.update(cx, |view, cx| view.focus(window, cx));
        window.draw(cx).clear(cx);
    });
    let reload = frame.update(|_, cx| {
        view.read(cx)
            .model
            .read(cx)
            .speech_input_intent()
            .expect("reload")
    });
    press(&mut frame, "tab tab tab enter");
    frame.run_until_parked();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::LoadSpeechInput))
    );
    deliver(
        &owner,
        vec![LinkEvent::Error {
            generation: 7,
            error: ErrorKey::HoldToTalkOff.into(),
        }],
        cx,
    );
    frame.update(|_, cx| assert!(view.read(cx).model.read(cx).speech_input_pending()));
    let mut off = saved.clone();
    off.hold_to_talk = false;
    deliver(
        &owner,
        vec![LinkEvent::Settings {
            generation: 7,
            view: off,
            answer: true,
        }],
        cx,
    );
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("speech-model-refused").is_some());
    assert!(frame.debug_bounds("speech-model-pending").is_none());
    frame.update(|_, cx| {
        let model = view.read(cx).model.read(cx);
        assert_eq!(
            cx.global::<crate::strings::Strings>()
                .error(model.speech_input_error().expect("load was refused")),
            "Hold to talk is off. Turn it on in Settings → Voice."
        );
    });
    deliver(
        &owner,
        vec![LinkEvent::Settings {
            generation: 7,
            view: saved.clone(),
            answer: false,
        }],
        cx,
    );
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("speech-model-refused").is_none());
    for (installed, status) in [
        (false, Text::VoiceNotInstalledFailed),
        (true, Text::VoiceInstalledFailed),
    ] {
        deliver(
            &owner,
            vec![LinkEvent::Voice {
                generation: 7,
                speech_input: PartState::Failed {
                    key: ErrorKey::VoiceLoad,
                    installed,
                },
            }],
            cx,
        );
        frame.run_until_parked();
        frame.update(|window, cx| {
            window.draw(cx).clear(cx);
            let model = view.read(cx).model.read(cx);
            assert_eq!(model.speech_input_status(), status);
            assert_eq!(
                cx.global::<crate::strings::Strings>()
                    .error(&model.speech_input_failure().expect("load failure")),
                "Voice couldn't start."
            );
        });
    }
    owner.update(cx, |owner, cx| owner.speech_input_action(reload, cx));
    let retry = frame.update(|_, cx| {
        view.read(cx)
            .model
            .read(cx)
            .speech_input_intent()
            .expect("retry")
    });
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(8, Incarnation(1)));
        owner.speech_input_action(retry, cx);
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    deliver(
        &owner,
        vec![
            LinkEvent::State(LinkState::Reconnecting),
            LinkEvent::Connected(Session::detached(8, Incarnation(1))),
            LinkEvent::Settings {
                generation: 8,
                view: saved,
                answer: false,
            },
            LinkEvent::Voice {
                generation: 8,
                speech_input: PartState::Failed {
                    key: ErrorKey::VoiceLoad,
                    installed: true,
                },
            },
        ],
        cx,
    );
    owner.update(cx, |owner, cx| owner.speech_input_action(retry, cx));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    let fresh = frame.update(|_, cx| {
        view.read(cx)
            .model
            .read(cx)
            .speech_input_intent()
            .expect("fresh retry")
    });
    owner.update(cx, |owner, cx| {
        owner.session = None;
        owner.speech_input_action(fresh, cx);
    });
    frame.update(|_, cx| {
        let model = view.read(cx).model.read(cx);
        assert!(model.speech_input_send_failed());
        assert!(!model.speech_input_pending());
    });
    drop(controls);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(8, Incarnation(1)));
        owner.speech_input_action(fresh, cx);
    });
    frame.run_until_parked();
    frame.update(|window, cx| window.draw(cx).clear(cx));
    assert!(frame.debug_bounds("speech-model-send-failed").is_some());
    let sent = owner.update(cx, |owner, cx| {
        let sent = owner.link.record_controls();
        owner.speech_input_action(fresh, cx);
        sent
    });
    assert_eq!(
        sent.try_recv(),
        Ok(ControlToBrain::Settings(SettingsOp::InstallVoice(
            SpeechInput
        )))
    );
    assert_eq!(sent.try_recv(), Err(async_channel::TryRecvError::Empty));
    frame.update(|_, cx| assert!(view.read(cx).model.read(cx).speech_input_pending()));
}

#[gpui::test]
fn the_privacy_tab_holds_the_screen_toggle_which_sends_once_and_waits_for_the_saved_answer(
    cx: &mut TestAppContext,
) {
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SettingsOp};

    let (window, view) = open(cx);
    let owner = crate::presence::test_fixture::presence(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    frame.update(|window, cx| {
        assert_eq!(
            view.read(cx).model.read(cx).show_screen_choice(),
            Some(true)
        );
        window.draw(cx).clear(cx);
    });
    // The Voice tab no longer holds it.
    assert_eq!(frame.debug_bounds("show-screen"), None);
    let tab = frame
        .debug_bounds("settings-privacy-tab")
        .expect("the Privacy tab")
        .center();
    frame.simulate_click(tab, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    frame.update(|window, cx| {
        assert_eq!(view.read(cx).tab, 3);
        window.draw(cx).clear(cx);
    });
    let toggle = frame
        .debug_bounds("show-screen")
        .expect("the toggle on the Privacy tab")
        .center();
    frame.simulate_click(toggle, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::Settings(SettingsOp::SetShowScreen(false))
    );
    frame.update(|_, cx| {
        let state = view.read(cx).model.read(cx);
        assert!(state.show_screen_pending());
        assert_eq!(state.show_screen_choice(), Some(false));
        assert!(state.view().unwrap().show_screen);
    });
    // A second click while the first waits sends nothing.
    frame.update(|window, cx| window.draw(cx).clear(cx));
    frame.simulate_click(toggle, gpui_kit::Modifiers::default());
    frame.run_until_parked();
    assert!(controls.is_empty());
    frame.update(|_, cx| {
        let mut saved = view.read(cx).model.read(cx).view().unwrap().clone();
        saved.show_screen = false;
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Settings {
                        generation: 7,
                        view: saved,
                        answer: true,
                    }]),
                    cx,
                )
                .unwrap();
        });
        let state = view.read(cx).model.read(cx);
        assert!(!state.show_screen_pending());
        assert_eq!(state.show_screen_choice(), Some(false));
    });
    assert!(controls.is_empty());
}

/// Which Privacy help the frame draws.
fn shown(frame: &mut gpui_kit::VisualTestContext) -> Vec<&'static str> {
    frame.update(|window, cx| window.draw(cx).clear(cx));
    ["show-screen-help", "show-screen-help-claude-code"]
        .into_iter()
        .filter(|selector| frame.debug_bounds(selector).is_some())
        .collect()
}
/// A connected presence whose default is Claude Code's Haiku, under the brain's offer for
/// it, with its Privacy tab open and a count of the tab's redraw requests.
fn privacy_under_claude_code(
    cx: &mut TestAppContext,
) -> (
    crate::presence::screen_hold_tests::Ready,
    gpui_kit::VisualTestContext,
    std::rc::Rc<std::cell::Cell<usize>>,
    gpui_kit::Subscription,
) {
    use crate::link::LinkEvent;
    use crate::presence::screen_hold_tests::{receive, screen_ready};
    use gpui_kit::{
        AppContext as _, Bounds, Point, VisualTestContext, WindowBounds, WindowOptions, px, size,
    };
    use kumochi_protocol::message::{
        ConnectionKind, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug, NoticeRevision,
        ScreenHolds,
    };

    let haiku = ModelInfo {
        slug: ModelSlug::new("haiku").expect("valid"),
        display_name: ModelName::new("Claude Haiku").expect("valid"),
        supported_efforts: None,
    };
    let ready = screen_ready(None, cx);
    let owner = ready.owner.clone();
    let (model, weak, events) = owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |settings, _| {
            settings.set_models(ConnectionKind::ClaudeCode, vec![haiku.clone()]);
            let mut view = settings.view().expect("connected").clone();
            view.default_model = Some(ModelChoice {
                model: haiku.slug.clone(),
                effort: Effort::Low,
                connection: ConnectionKind::ClaudeCode,
            });
            view.show_screen = true;
            settings.set_view(view, crate::settings::Update::Unsolicited);
        });
        (
            owner.settings.clone(),
            cx.weak_entity(),
            owner.events.clone(),
        )
    });
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(480.), px(640.)),
        })),
        ..Default::default()
    };
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                let mut view = super::super::SettingsView::new(model, weak, events, window, cx);
                view.tab = 3;
                view
            })
        })
        .expect("settings window")
    });
    receive(
        &owner,
        LinkEvent::ScreenHolds {
            generation: 1,
            state: ScreenHolds::On {
                model: haiku.display_name,
                connection: ConnectionKind::ClaudeCode,
                slug: haiku.slug,
                revision: NoticeRevision::FIRST,
            },
        },
        cx,
    );
    let mut frame = VisualTestContext::from_window(window, cx);
    frame.run_until_parked();
    assert_eq!(shown(&mut frame), ["show-screen-help-claude-code"]);
    let notified = std::rc::Rc::new(std::cell::Cell::new(0));
    let watch = cx.update(|cx| {
        let notified = std::rc::Rc::clone(&notified);
        cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
    });
    (ready, frame, notified, watch)
}

/// The Privacy tab's help follows the offer the shortcut could be seen under now: unchecking the
/// box, or asking for another default model, ends Claude Code's words before the brain replies, as
/// a removed offer or a lost connection does, and each change repaints the open tab.
#[gpui::test]
fn the_privacy_help_follows_the_shortcuts_current_offer(cx: &mut TestAppContext) {
    use crate::link::{LinkEvent, LinkState};
    use crate::presence::screen_hold_tests::{luna, on, receive};
    use kumochi_protocol::message::{ConnectionKind, ScreenHolds, ScreenOff};

    let unchecked = {
        let (_ready, mut frame, notified, _watch) = privacy_under_claude_code(cx);
        let point = frame
            .debug_bounds("show-screen")
            .expect("the checkbox")
            .center();
        frame.simulate_click(point, gpui_kit::Modifiers::default());
        frame.run_until_parked();
        (notified.get(), shown(&mut frame))
    };
    assert!(unchecked.0 > 0, "the click repaints the tab");
    assert_eq!(unchecked.1, ["show-screen-help"], "unchecked, still saving");

    let another_default = {
        let (ready, mut frame, notified, _watch) = privacy_under_claude_code(cx);
        ready.owner.update(cx, |owner, cx| {
            owner.save_default_model(ConnectionKind::ChatGpt, luna().slug, cx);
        });
        frame.run_until_parked();
        (notified.get(), shown(&mut frame))
    };
    assert!(another_default.0 > 0, "the new default repaints the tab");
    assert_eq!(
        another_default.1,
        ["show-screen-help"],
        "another default, still saving"
    );

    for (case, event) in [
        (
            "the offer moved to another route",
            LinkEvent::ScreenHolds {
                generation: 1,
                state: on(&luna()),
            },
        ),
        (
            "the offer was withdrawn",
            LinkEvent::ScreenHolds {
                generation: 1,
                state: ScreenHolds::Off(ScreenOff::NotReady),
            },
        ),
        (
            "the connection was lost",
            LinkEvent::State(LinkState::Reconnecting),
        ),
    ] {
        let (ready, mut frame, notified, _watch) = privacy_under_claude_code(cx);
        receive(&ready.owner, event, cx);
        frame.run_until_parked();
        assert!(notified.get() > 0, "{case}: repaints the tab");
        assert_eq!(shown(&mut frame), ["show-screen-help"], "{case}");
    }
}
