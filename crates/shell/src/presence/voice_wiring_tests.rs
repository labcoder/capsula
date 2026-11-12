use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, GestureId, HoldFailure, HoldStatus, Incarnation, PartState,
    ScreenChoice, ThreadId,
};

use super::{Presence, test_fixture, voice::Stamp};
use crate::input::hold::Intent;
use crate::link::{Batch, LinkEvent, LinkHandle, Session};
use crate::platform::microphone::{Generation, Mode};
use crate::strings::Text;

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
                    main_thread: ThreadId(1),
                    max_frame_rate: kumochi_protocol::message::MaxFrameRate::AUTOMATIC,
                    default_model: None,
                    dock_position: kumochi_protocol::message::DockPosition::TopCenter,
                    show_in_taskbar: false,
                    talk_back: false,
                    stt_unload: kumochi_protocol::message::SttUnload::Kept,
                    hold_to_talk: true,
                },
                crate::settings::Update::Unsolicited,
            );
            settings.receive_speech_input(Some(1), 1, PartState::Ready);
        });
        owner.runs.met(Incarnation(1));
        owner.runs.connected(1);
        owner.runs.settings(ThreadId(1));
        owner.refresh_conversation(cx);
        (data, controls)
    });
    (owner, data, controls)
}

pub(super) fn hold(
    owner: &Entity<Presence>,
    id: u32,
    cx: &mut TestAppContext,
) -> (Generation, Stamp) {
    owner.update(cx, |owner, cx| {
        let gesture = GestureId(id);
        let token = owner
            .link
            .microphone_gate()
            .arm(gesture)
            .expect("fresh hold");
        owner.link.track_input(gesture);
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture,
                generation: Ok(token.clone()),
            },
            stamp,
            0,
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
    let (owner, _data, _controls) = ready(cx);
    let (token, _) = hold(&owner, 1, cx);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    owner.update(cx, |owner, cx| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
        assert!(!owner.dock.read(cx).listening);
        assert!(!owner.link.input_is(GestureId(1)));
        assert!(token.retired());
        assert_eq!(owner.runs.take_hold_cancel(), None);
        assert_eq!(
            owner.runs.begin_hold(GestureId(2)),
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
                assert!(settings.receive_speech_input(Some(1), 1, state));
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
        owner.update(cx, |owner, cx| {
            assert!(owner.runs.select_thread(Some(ThreadId(9))));
            owner.refresh_conversation(cx);
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
                cx,
            );
            assert!(owner.voice.pending.is_none());
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
    let (owner, _data, _controls) = ready(cx);
    let (_old, old_stamp) = hold(&owner, 1, cx);
    owner.update(cx, Presence::cancel_voice);
    let (new, _) = hold(&owner, 2, cx);
    owner.update(cx, |owner, cx| {
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: true,
            },
            old_stamp,
            0,
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
            .native_escape(Some(GestureId(1)), cx)
            .expect("cancel voice only");
        assert!(owner.voice.pending.is_none());
        assert!(owner.text_open());
        owner
            .native_escape(Some(GestureId(1)), cx)
            .expect("consume a deferred native marker");
        assert!(owner.text_open());
    });
}
