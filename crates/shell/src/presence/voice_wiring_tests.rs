use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, GestureId, HoldFailure, HoldStatus, Incarnation, PartState,
    ScreenChoice, ThreadId,
};

use super::{Presence, test_fixture, voice::Stamp};
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::link::{Batch, LinkEvent, LinkHandle, Session};
use crate::platform::microphone::{Generation, Mode};
use crate::strings::Text;

#[cfg(windows)]
#[gpui::test]
fn actual_quit_revokes_legacy_capture_without_waiting_for_the_outbox(cx: &mut TestAppContext) {
    use crate::link::fixture;
    use crate::platform::microphone::MicError;

    let pipe = fixture::attended("legacy-presence-quit");
    let events = fixture::patient(pipe.events.clone());
    let session = fixture::connected(fixture::next(&events));
    let owner = test_fixture::presence(cx);
    cx.update(|cx| {
        cx.set_global(super::PresenceApp {
            _owner: owner.clone(),
        });
    });
    let gesture = GestureId(7);
    let token = pipe.handle.microphone_gate().arm(gesture).unwrap();
    pipe.handle.track_input(gesture);
    owner.update(cx, |this, _| {
        this.link = pipe.handle.clone();
        this.session = Some(session.clone());
    });
    fixture::with_outbox_held(&pipe.handle, &session, || {
        owner.update(cx, Presence::quit);
        assert!(!pipe.handle.input_is(gesture));
        assert_eq!(
            token.mode(&session.alive(), Instant::now()),
            Err(MicError::Cancelled)
        );
        owner.read_with(cx, |this, _| assert!(this.voice.paused.get()));
        assert!(
            !pipe.handle.send_data(
                &session,
                DataToBrain::MicPcm(kumochi_protocol::message::MicPcm {
                    gesture,
                    seq: 0,
                    rate: 48_000,
                    last: false,
                    samples: vec![5; 960],
                },)
            ),
            "stale PCM does not wait for the outbox after Quit"
        );
    });
    drop(pipe);
    cx.run_until_parked();
}

pub(super) fn ready(
    cx: &mut TestAppContext,
) -> (
    Entity<Presence>,
    async_channel::Receiver<DataToBrain>,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let (data, controls) = owner.update(cx, |owner, cx| {
        let (mut link, data) = LinkHandle::recording();
        let controls = link.record_controls();
        owner.link = link;
        owner.session = Some(Session::detached(1, Incarnation(1)));
        owner.settings.update(cx, |settings, _| {
            settings.set_view(
                kumochi_protocol::message::SettingsView {
                    assistant_name: kumochi_protocol::message::AssistantName::new(
                        kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
                    )
                    .unwrap(),
                    user_name: None,
                    onboarded: true,
                    onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
                    main_thread: ThreadId(1),
                    max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
                    default_model: None,
                    dock_position: kumochi_protocol::message::DockPosition::TopCenter,
                    show_in_taskbar: false,
                    talk_back: false,
                    stt_unload: kumochi_protocol::message::SttUnload::Kept,
                    hold_to_talk: true,
                    screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
                    show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
                },
                crate::settings::Update::Unsolicited,
            );
            settings.receive_voice(
                Some(1),
                1,
                kumochi_protocol::message::VoiceView {
                    speech_input: PartState::Ready,
                    spoken_replies: PartState::Ready,
                },
            );
        });
        owner.runs.met(Incarnation(1));
        owner.runs.connected(1);
        owner.runs.settings(ThreadId(1));
        owner.refresh_conversation(cx);
        owner.follow_voice_input(cx);
        owner.forward_changes(cx);
        (data, controls)
    });
    (owner, data, controls)
}

/// Takes gesture `id`'s capture inhibit as the native callback does at key-down, unless it has a
/// gate already, then marks its hold recognized, as the callback does before the deferred
/// delivery.
pub(super) fn gate(owner: &Presence, id: u32, cx: &gpui_kit::App) {
    key_down(owner, id, cx);
    owner.voice.gates.recognized(GestureId(id), true);
}

/// Gesture `id`'s key-down in the native callback, unless it has a gate already.
pub(super) fn key_down(owner: &Presence, id: u32, cx: &gpui_kit::App) {
    owner.follow_voice_input(cx);
    if owner.voice.gates.held(GestureId(id)).is_none() {
        owner.voice.gates.warm(
            owner.link.output(),
            GestureId(id),
            owner.link.link_generation(),
            owner.link.choice_revision(),
        );
    }
}

pub(super) fn hold(
    owner: &Entity<Presence>,
    id: u32,
    cx: &mut TestAppContext,
) -> (Generation, Stamp) {
    owner.update(cx, |owner, cx| {
        let gesture = GestureId(id);
        gate(owner, id, cx);
        let token = owner
            .link
            .microphone_gate()
            .arm(gesture)
            .expect("fresh hold");
        owner.link.track_input(gesture);
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: owner.session.as_ref().map_or(1, Session::generation),
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture,
                generation: Ok(token.clone()),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert!(owner.voice.pending.is_some());
        (token, stamp)
    })
}

pub(super) fn status(
    owner: &Entity<Presence>,
    generation: u64,
    gesture: u32,
    status: HoldStatus,
    cx: &mut TestAppContext,
) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::VoiceHold {
                    generation,
                    gesture: GestureId(gesture),
                    status,
                }]),
                cx,
            )
            .expect("voice status batch");
    });
}

#[gpui::test]
fn a_pre_run_failure_clears_only_its_matching_hold(cx: &mut TestAppContext) {
    test_fixture::without_desktop(cx);
    let (owner, _data, _controls) = ready(cx);
    let (token, _) = hold(&owner, 1, cx);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    owner.update(cx, |owner, cx| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
        assert!(!owner.dock.read(cx).listening);
        assert!(!owner.link.input_is(GestureId(1)));
        assert!(token.retired());
        assert_eq!(owner.runs.take_hold_cancel(), None);
        assert_eq!(
            owner.runs.begin_hold(GestureId(2), None),
            Ok(ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(2),
                thread: Some(ThreadId(1)),
                selection: 1,
            })
        );
    });
}

#[gpui::test]
fn not_loaded_distinguishes_missing_and_unloaded_without_opening_microphone(
    cx: &mut TestAppContext,
) {
    test_fixture::without_desktop(cx);
    for (state, expected) in [
        (PartState::Unloaded, Text::VoiceUnloaded),
        (PartState::NotInstalled, Text::VoiceNotInstalled),
    ] {
        let (owner, data, _controls) = ready(cx);
        let starts = Rc::new(Cell::new(0));
        let attempts = Rc::clone(&starts);
        let (_token, _) = hold(&owner, 1, cx);
        owner.update(cx, |owner, cx| {
            owner.voice.start = Some(Box::new(move |_, _| {
                attempts.set(attempts.get() + 1);
                Err(crate::platform::microphone::MicError::Unavailable)
            }));
            owner.settings.update(cx, |settings, _| {
                assert!(settings.receive_voice(
                    Some(1),
                    1,
                    kumochi_protocol::message::VoiceView {
                        speech_input: state,
                        spoken_replies: PartState::Ready
                    }
                ));
            });
        });
        status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::NotLoaded), cx);
        owner.read_with(cx, |owner, _| {
            assert_eq!(owner.voice.status, Some(expected));
            assert!(owner.voice.pending.is_none());
        });
        assert_eq!(starts.get(), 0);
        assert!(data.is_empty());
    }
}

#[gpui::test]
fn stale_session_and_gesture_status_cannot_retarget_or_finish_input(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (token, _) = hold(&owner, 1, cx);
    status(&owner, 2, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    status(&owner, 1, 2, HoldStatus::Cancelled, cx);
    status(&owner, 1, 1, HoldStatus::Loading, cx);
    owner.update(cx, |owner, cx| {
        assert_eq!(owner.voice.status, Some(Text::VoiceLoading));
        assert_eq!(owner.voice.target.get(), (Some(ThreadId(1)), 1));
        assert_eq!(
            (owner.runs.thread(), owner.runs.revision()),
            (Some(ThreadId(1)), 1)
        );
        assert!(owner.voice.pending.is_some());
        assert!(owner.link.input_is(GestureId(1)));
        assert!(!owner.dock.read(cx).listening);
        assert_eq!(
            token.mode(
                &owner.session.as_ref().expect("session").alive(),
                Instant::now()
            ),
            Ok(Mode::Preparing)
        );
    });
}

#[gpui::test]
fn a_captured_target_or_session_cannot_be_rebound_at_delivery(cx: &mut TestAppContext) {
    for (target, session) in [((Some(ThreadId(1)), 1), 1), ((Some(ThreadId(9)), 2), 2)] {
        let (owner, _data, _controls) = ready(cx);
        let token = owner.update(cx, |owner, cx| {
            assert!(owner.runs.select_thread(Some(ThreadId(9))));
            owner.refresh_conversation(cx);
            gate(owner, 1, cx);
            let token = owner
                .link
                .microphone_gate()
                .arm(GestureId(1))
                .expect("fresh hold");
            owner.link.track_input(GestureId(1));
            owner.voice_intent(
                Intent::Recognized {
                    gesture: GestureId(1),
                    generation: Ok(token.clone()),
                },
                Stamp {
                    target,
                    session,
                    choice: owner.link.choice_revision(),
                },
                0,
                None,
                TalkHeld::default(),
                cx,
            );
            assert!(owner.voice.pending.is_none());
            token
        });
        cx.run_until_parked();
        owner.update(cx, |owner, _| {
            assert!(token.retired());
            assert!(!owner.link.input_is(GestureId(1)));
            assert_eq!(owner.voice.target.get(), (Some(ThreadId(9)), 2));
            assert_eq!(
                (owner.runs.thread(), owner.runs.revision()),
                (Some(ThreadId(9)), 2)
            );
        });
    }
}

#[gpui::test]
fn late_status_and_release_cannot_cancel_a_newer_held_input(cx: &mut TestAppContext) {
    test_fixture::without_desktop(cx);
    let (owner, _data, _controls) = ready(cx);
    let (old, old_stamp) = hold(&owner, 1, cx);
    owner.update(cx, Presence::cancel_voice);
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(old.retired());
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
        assert!(!owner.link.voice_stop_pending());
    });
    let (new, _) = hold(&owner, 2, cx);
    owner.update(cx, |owner, cx| {
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: true,
            },
            old_stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
    });
    status(&owner, 1, 1, HoldStatus::Empty, cx);
    owner.update(cx, |owner, cx| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert!(owner.voice.pending.is_some());
        assert!(owner.link.input_is(GestureId(2)));
        assert!(!owner.dock.read(cx).listening);
        assert_eq!(
            new.mode(
                &owner.session.as_ref().expect("session").alive(),
                Instant::now()
            ),
            Ok(Mode::Preparing)
        );
    });
}

#[gpui::test]
fn one_typed_send_cancels_and_drains_a_hold_before_submitting(cx: &mut TestAppContext) {
    test_fixture::without_desktop(cx);
    let (owner, data, _controls) = ready(cx);
    let (token, _) = hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        let text = owner.text.as_ref().expect("typing fixture");
        text.window
            .update(cx, |_, window, cx| {
                text.input
                    .update(cx, |input, cx| input.set_value("typed turn", window, cx));
            })
            .expect("typing window");
        owner.submit(cx).expect("one Send");
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.runs.take_hold_cancel(), None);
    });
    cx.run_until_parked();
    owner.update(cx, |owner, cx| {
        assert!(token.retired());
        assert!(!owner.link.input_is(GestureId(1)));
        assert_eq!(
            owner
                .text
                .as_ref()
                .expect("typing fixture")
                .input
                .read(cx)
                .value()
                .as_ref(),
            ""
        );
    });
    assert_eq!(
        data.try_recv().expect("typed turn sent on the first click"),
        DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "typed turn".into(),
        }
    );
    assert!(data.is_empty());
}

#[gpui::test]
fn escape_consumes_the_native_input_marker_without_dismissing_typing(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (_token, _) = hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.text.as_mut().expect("typing fixture").typing.open();
        owner.link.cancel_input();
        owner
            .native_escape(Some(GestureId(1)), None, cx)
            .expect("cancel voice only");
        assert!(owner.voice.pending.is_none());
        assert!(owner.text_open());
        owner
            .native_escape(Some(GestureId(1)), None, cx)
            .expect("consume a deferred native marker");
        assert!(owner.text_open());
    });
}

#[gpui::test]
fn screen_hold_events_are_stored_from_this_connection_only_and_change_no_hold(
    cx: &mut TestAppContext,
) {
    use crate::runs::{ScreenHold, ScreenLine};
    use kumochi_protocol::message::{HeldScreen, ModelName, ScreenClass, ScreenHolds};
    let (owner, _data, controls) = ready(cx);
    let on = ScreenHolds::On {
        model: ModelName::new("GPT-5.5").unwrap(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: kumochi_protocol::message::ModelSlug::new("gpt-5.5").unwrap(),
        revision: kumochi_protocol::message::NoticeRevision::FIRST,
    };
    let apply = |event, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner
                .link_events(Batch::Plain(vec![event]), cx)
                .expect("screen hold batch");
        });
    };
    let state = |generation, cx: &mut TestAppContext| {
        apply(
            LinkEvent::ScreenHolds {
                generation,
                state: on.clone(),
            },
            cx,
        );
    };
    state(2, cx);
    owner.read_with(cx, |owner, _| assert_eq!(owner.runs.screen_holds(), None));
    state(1, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.screen_holds(), Some(&on));
    });

    // The state is stored and nothing reads it yet: a hold is still the voice-only message.
    let (_token, _) = hold(&owner, 1, cx);
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: Some(ThreadId(1)),
            selection: 1,
        }
    );
    assert!(controls.is_empty());
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);

    let outcome = |generation, cx: &mut TestAppContext| {
        apply(
            LinkEvent::HoldScreen {
                generation,
                gesture: GestureId(2),
                outcome: HeldScreen::NotIncluded(ScreenClass::Changed),
            },
            cx,
        );
    };
    owner.update(cx, |owner, _| {
        let screen = ScreenHold {
            pointer: kumochi_protocol::geometry::PhysicalPoint([3, 4]),
            lease: kumochi_protocol::message::LeaseGeneration(1),
            t_qpc: 5,
        };
        owner.runs.begin_hold(GestureId(2), Some(screen)).unwrap();
    });
    outcome(2, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.hold_screen_line(), Some(ScreenLine::WillSee));
    });
    outcome(1, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.runs.hold_screen_line(),
            Some(ScreenLine::NotIncluded(ScreenClass::Changed))
        );
    });
}
