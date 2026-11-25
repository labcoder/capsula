use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{MicPcm, RunId};

use super::super::voice_wiring_tests::{hold, ready, status};
use super::*;
use crate::input::{Edge, Event, Key};
use crate::link::{Batch, LinkEvent};
use crate::platform::microphone::{MicError, Running};
use crate::platform::{Deliver, EventTime};

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture() -> (
    Captured,
    tracing::subscriber::DefaultGuard,
    tracing::Dispatch,
) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let logging = tracing::subscriber::set_default(subscriber);
    (captured, logging, second)
}

#[gpui::test]
fn hold_failures_log_closed_stages_and_reasons_without_user_content(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let (captured, _logging, _second) = capture();
    for (worker, error) in [
        (false, MicError::Unavailable),
        (true, MicError::Format),
        (true, MicError::Deadline),
        (true, MicError::Stale),
        (true, MicError::Discontinuity),
        (true, MicError::Timestamp),
        (true, MicError::Overflow),
        (true, MicError::Unconfirmed),
    ] {
        let (owner, data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        owner.update(cx, |owner, _| {
            owner.voice.start = Some(Box::new(move |token, session| {
                if worker {
                    MicOwner::start_reserved_with_for_tests(token, session, move |_, _, _, _| {
                        if error == MicError::Unconfirmed {
                            Err(error)
                        } else {
                            Ok(Finalization::Discarded(error))
                        }
                    })
                } else {
                    Err(error)
                }
            }));
        });
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        until(cx, |cx| {
            owner.read_with(cx, |owner, _| {
                if error == MicError::Unconfirmed {
                    owner
                        .voice
                        .pending
                        .as_ref()
                        .is_some_and(|hold| hold.terminal == Some(Text::HoldUnavailable))
                } else {
                    owner.voice.pending.is_none()
                }
            })
        });
        assert!(data.is_empty());
    }
    let (owner, _data, _controls) = ready(cx);
    let (_, stamp) = hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.voice_intent(Intent::Unavailable, stamp, 0, None, TalkHeld::default(), cx);
    });
    for reason in [
        HoldFailure::NotLoaded,
        HoldFailure::Busy,
        HoldFailure::Unavailable,
        HoldFailure::InvalidAudio,
        HoldFailure::TooLong,
        HoldFailure::TimedOut,
        HoldFailure::ChoiceChanged,
        HoldFailure::DecodeFailed,
    ] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        status(&owner, 1, 1, HoldStatus::Failed(reason), cx);
    }
    let bytes = captured.0.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    let diagnostics = text
        .lines()
        .filter_map(|line| {
            line.split("held speech input failed ")
                .nth(1)
                .or_else(|| line.split("held speech input ended ").nth(1))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        [
            "stage=CaptureStart reason=Unavailable",
            "stage=CaptureReceipt reason=Format",
            "stage=CaptureReceipt reason=Deadline",
            "stage=CaptureReceipt reason=Stale",
            "stage=CaptureReceipt reason=Discontinuity",
            "stage=CaptureReceipt reason=Timestamp",
            "stage=CaptureReceipt reason=Overflow",
            "stage=CaptureReceipt reason=Unconfirmed",
            "stage=Input reason=GestureUnavailable",
            "stage=Brain reason=NotLoaded",
            "stage=Brain reason=Busy",
            "stage=Brain reason=Unavailable",
            "stage=Brain reason=InvalidAudio",
            "stage=Brain reason=TooLong",
            "stage=Brain reason=TimedOut",
            "stage=Brain reason=ChoiceChanged",
            "stage=Brain reason=DecodeFailed",
        ]
    );
}

#[gpui::test]
fn silent_hold_admission_exits_report_why_without_identifiers(cx: &mut TestAppContext) {
    let (captured, _logging, _second) = capture();
    for cut in 0..5 {
        let (owner, _data, _controls) = ready(cx);
        owner.update(cx, |owner, cx| {
            crate::presence::voice_wiring_tests::gate(owner, 7, cx);
            let token = owner.link.microphone_gate().arm(GestureId(7)).unwrap();
            owner.link.track_input(GestureId(7));
            let mut stamp = Stamp {
                target: owner.voice.target.get(),
                session: 1,
                choice: owner.link.choice_revision(),
            };
            match cut {
                0 => owner.session = None,
                1 => stamp.session = 2,
                2 => owner.link.clear_input(GestureId(7)),
                3 => owner.link.microphone_gate().release(GestureId(7)),
                4 => {
                    owner.runs.disconnected();
                    stamp.target = (owner.runs.thread(), owner.runs.revision());
                }
                _ => unreachable!("five admission cuts"),
            }
            owner.voice_intent(
                Intent::Recognized {
                    gesture: GestureId(7),
                    generation: Ok(token),
                },
                stamp,
                0,
                None,
                TalkHeld::default(),
                cx,
            );
            assert!(owner.voice.pending.is_none());
        });
    }
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let diagnostics = text
        .lines()
        .filter_map(|line| {
            line.split("held speech input refused ")
                .nth(1)
                .or_else(|| line.split("held speech input failed ").nth(1))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        [
            "stage=Admission reason=NoSession",
            "stage=Admission reason=StaleContext",
            "stage=Admission reason=InputFence",
            "stage=Admission reason=GenerationNotPreparing",
            "stage=Admission reason=Offline",
        ]
    );
}

#[gpui::test]
fn blocked_hold_chords_and_refused_collection_arms_log_no_key_codes(cx: &mut TestAppContext) {
    let (captured, _logging, _second) = capture();
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    deliver(
        Event::Key {
            key: Key::Other(crate::input::PhysicalKey::new(42).unwrap()),
            edge: Edge::Down,
        },
        EventTime::new(at),
    );
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Deadline,
        EventTime::new(at + Duration::from_millis(301)),
    );
    cx.run_until_parked();
    assert!(controls.is_empty());
    owner.read_with(cx, |owner, _| assert!(owner.voice.pending.is_none()));
    let (owner, _data, controls) = ready(cx);
    owner.read_with(cx, |owner, _| {
        owner.link.microphone_gate().arm(GestureId(7)).unwrap();
    });
    let deliver = native(&owner, cx);
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Deadline,
        EventTime::new(at + Duration::from_millis(301)),
    );
    cx.run_until_parked();
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::KeyDown { .. }
    ));
    assert!(controls.is_empty());
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
    });
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let diagnostics = text
        .lines()
        .filter_map(|line| {
            line.split("held speech input boundary ")
                .nth(1)
                .or_else(|| line.split("held speech input refused ").nth(1))
                .or_else(|| line.split("held speech input failed ").nth(1))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        [
            "stage=Gesture reason=ChordSuppressed",
            "stage=Gesture reason=ChordStarted",
            "stage=Gesture reason=HoldRecognized by=Deadline",
            "stage=Gesture reason=Stale",
            "stage=Admission reason=Stale",
        ]
    );
}

#[gpui::test]
fn a_recognised_hold_logs_whether_the_deadline_a_repeat_or_the_release_reached_it(
    cx: &mut TestAppContext,
) {
    let (captured, _logging, _second) = capture();
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Key {
            key: Key::LeftAlt,
            edge: Edge::Down,
        },
        after(at, 500),
    );
    chord(&deliver, Edge::Up, at + Duration::from_millis(900));
    let at = at + Duration::from_secs(2);
    chord(&deliver, Edge::Down, at);
    chord(&deliver, Edge::Up, at + Duration::from_millis(400));
    cx.run_until_parked();
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let recognised = text
        .lines()
        .filter_map(|line| line.split("reason=HoldRecognized ").nth(1))
        .collect::<Vec<_>>();
    assert_eq!(recognised, ["by=Repeat", "by=Release"]);
}

#[gpui::test]
fn a_hold_whose_native_deadline_never_arrives_is_recognised_by_the_scheduled_one(
    cx: &mut TestAppContext,
) {
    let (captured, _logging, _second) = capture();
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = cx.background_executor.now();
    chord(&deliver, Edge::Down, at);
    cx.run_until_parked();
    cx.background_executor
        .advance_clock(Duration::from_millis(299));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| assert!(owner.voice.pending.is_none()));
    cx.background_executor
        .advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert_eq!(
            owner
                .voice
                .pending
                .as_ref()
                .map(|hold| hold.token.gesture()),
            Some(GestureId(1))
        );
    });
    deliver(Event::Deadline, after(at, 340));
    cx.run_until_parked();
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let recognised = text
        .lines()
        .filter_map(|line| line.split("reason=HoldRecognized ").nth(1))
        .collect::<Vec<_>>();
    assert_eq!(recognised, ["by=Scheduled"]);
}

pub(super) fn native(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Deliver {
    let weak = owner.downgrade();
    owner.update(cx, |owner, cx| {
        super::super::gesture::deliver(
            owner.dock_window,
            weak,
            owner.link.clone(),
            Rc::clone(&owner.voice.target),
            Rc::clone(&owner.voice.paused),
            owner.voice.gates.clone(),
            cx,
        )
    })
}

pub(super) fn chord(deliver: &Deliver, edge: Edge, at: Instant) {
    for key in [Key::LeftControl, Key::LeftAlt] {
        let time = EventTime::new(at);
        let time = if key == Key::LeftAlt && edge == Edge::Down {
            time.with_keyboard_read(Some)
        } else {
            time
        };
        deliver(Event::Key { key, edge }, time);
    }
}

#[gpui::test]
fn a_successful_drawing_and_focus_keep_the_next_native_hold_admissible(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    owner.update(cx, |owner, cx| {
        assert!(owner.runs.select_thread(None));
        owner.runs.submit("draw a shape", |_| true).unwrap();
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::RunState {
                        generation: 1,
                        run: RunId(1),
                        thread: ThreadId(9),
                        phase: Phase::Thinking,
                        error: None,
                        origin: Some(RunOrigin::Typed { turn: 1 }),
                    },
                    LinkEvent::Focus {
                        generation: 1,
                        run: RunId(1),
                        thread: ThreadId(9),
                    },
                    LinkEvent::RunState {
                        generation: 1,
                        run: RunId(1),
                        thread: ThreadId(9),
                        phase: Phase::Done,
                        error: None,
                        origin: Some(RunOrigin::Typed { turn: 1 }),
                    },
                ]),
                cx,
            )
            .unwrap();
        owner.presenter =
            crate::presenter::tests::painter_fixture(Box::new(crate::presenter::tests::Charged(0)))
                .0;
        owner.presenter.frames().borrow_mut().shown = true;
        owner.runs_changed(cx).unwrap();
        assert!(owner.activity.drawing);
        assert_eq!(owner.runs.thread(), Some(ThreadId(9)));
    });
    let at = Instant::now();
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Deadline,
        EventTime::new(at + Duration::from_millis(301)),
    );
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert!(owner.voice.pending.is_some());
    });
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::KeyDown {
            gesture: GestureId(1),
            ..
        }
    ));
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: Some(ThreadId(9)),
            selection: 3,
        }
    );
    assert!(controls.is_empty());
}

#[gpui::test]
fn microphone_stop_acceptance_wait_keeps_typing_and_does_not_open_another_source(
    cx: &mut TestAppContext,
) {
    let (owner, data, controls) = ready(cx);
    let (old, _) = hold(&owner, 1, cx);
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            ..
        }
    ));
    status(&owner, 1, 1, HoldStatus::Cancelled, cx);
    cx.run_until_parked();
    assert!(old.retired());
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::GestureCancel {
            gesture: GestureId(1)
        }
    );
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1)
        }
    );
    let starts = Rc::new(Cell::new(0));
    let attempts = Rc::clone(&starts);
    let token = owner.update(cx, |owner, cx| {
        let text = owner.text.as_mut().unwrap();
        text.typing.open();
        text.window
            .update(cx, |_, window, cx| {
                text.input
                    .update(cx, |input, cx| input.set_value("unsent draft", window, cx));
            })
            .unwrap();
        owner.voice.start = Some(Box::new(move |_, _| {
            attempts.set(attempts.get() + 1);
            Err(MicError::Unavailable)
        }));
        crate::presence::voice_wiring_tests::gate(owner, 2, cx);
        let token = owner.link.microphone_gate().arm(GestureId(2)).unwrap();
        owner.link.track_input(GestureId(2));
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture: GestureId(2),
                generation: Ok(token.clone()),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert!(owner.text_open());
        assert_eq!(
            owner.text.as_ref().unwrap().input.read(cx).value().as_str(),
            "unsent draft"
        );
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
        assert!(owner.voice.pending.is_none());
        assert!(owner.link.voice_stop_pending());
        token
    });
    cx.run_until_parked();
    assert!(token.retired());
    assert_eq!(starts.get(), 0);
    assert!(data.is_empty());
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(2)
        }
    );
    assert!(controls.is_empty());
}

#[gpui::test]
fn microphone_start_without_running_reports_unavailable_not_cancellation(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    for (worker, error) in [
        (true, MicError::Unavailable),
        (true, MicError::Format),
        (false, MicError::Unavailable),
    ] {
        let (owner, data, controls) = ready(cx);
        let (token, _) = hold(&owner, 1, cx);
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                thread: Some(ThreadId(1)),
                selection: 1,
            }
        );
        owner.update(cx, |owner, _| {
            owner.voice.start = Some(Box::new(move |token, session| {
                if worker {
                    MicOwner::start_reserved_with_for_tests(token, session, move |_, _, _, _| {
                        Ok(Finalization::Discarded(error))
                    })
                } else {
                    Err(error)
                }
            }));
        });
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        until(cx, |cx| {
            owner.read_with(cx, |owner, _| owner.voice.pending.is_none())
        });
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.voice.status.map(|text| cx.global::<crate::strings::Strings>().get(text)),
                Some("Microphone unavailable. Check microphone access and your input device, then try again.".to_owned()));
            assert!(!owner.dock.read(cx).listening);
            assert!(!owner.link.input_is(GestureId(1)));
        });
        assert!(data.is_empty());
        cx.run_until_parked();
        assert!(token.retired());
        let stopped = ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1),
        };
        let cancel = ControlToBrain::GestureCancel {
            gesture: GestureId(1),
        };
        // A worker's receipt tells the brain first; an unstarted generation's gate after.
        let (first, second) = if worker {
            (stopped, cancel)
        } else {
            (cancel, stopped)
        };
        assert_eq!(controls.try_recv().unwrap(), first);
        assert_eq!(controls.try_recv().unwrap(), second);
        assert!(controls.is_empty());
    }
}

#[gpui::test]
fn microphone_start_refusals_before_open_never_suggest_an_active_device(cx: &mut TestAppContext) {
    let (owner, data, _controls) = ready(cx);
    let (_token, stamp) = hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.voice_intent(
            Intent::Recognized {
                gesture: GestureId(2),
                generation: Err(MicError::Stale),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
        assert_eq!(
            owner.voice.pending.as_ref().unwrap().token.gesture(),
            GestureId(1)
        );
    });
    assert!(data.is_empty());
    let (owner, data, _controls) = ready(cx);
    let token = owner.update(cx, |owner, cx| {
        owner.runs.disconnected();
        crate::presence::voice_wiring_tests::gate(owner, 1, cx);
        let token = owner.link.microphone_gate().arm(GestureId(1)).unwrap();
        owner.link.track_input(GestureId(1));
        let stamp = Stamp {
            target: (owner.runs.thread(), owner.runs.revision()),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture: GestureId(1),
                generation: Ok(token.clone()),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
        assert!(owner.voice.pending.is_none());
        token
    });
    cx.run_until_parked();
    assert!(token.retired());
    assert!(data.is_empty());
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one refused-state flow checks retained surfaces, recovery and recording authority"
)]
fn rejected_speech_states_preserve_typing_recovery_and_local_notices(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{ErrorKey, PartState};
    for (state, notice) in [
        (Some(PartState::NotInstalled), Text::VoiceNotInstalled),
        (Some(PartState::Verifying), Text::VoiceVerifying),
        (
            Some(PartState::Downloading { done: 1, total: 2 }),
            Text::VoiceDownloading,
        ),
        (
            Some(PartState::Failed {
                key: ErrorKey::VoiceLoad,
                installed: false,
            }),
            Text::VoiceNotInstalledFailed,
        ),
        (Some(PartState::Unavailable), Text::VoiceRestartRequired),
        (None, Text::VoiceStatusUnknown),
    ] {
        let (owner, data, controls) = ready(cx);
        let starts = Rc::new(Cell::new(0));
        let attempts = Rc::clone(&starts);
        owner.update(cx, |owner, cx| {
            owner.runs.apply_scenes(&[]).unwrap();
            owner.runs.submit("recoverable typed", |_| true).unwrap();
            let text = owner.text.as_mut().unwrap();
            text.typing.open();
            text.window
                .update(cx, |_, window, cx| {
                    text.input
                        .update(cx, |input, cx| input.set_value("unsent draft", window, cx));
                })
                .unwrap();
            owner.settings.update(cx, |settings, _| {
                let saved = settings.view().unwrap().clone();
                settings.disconnected();
                settings.set_view(saved, crate::settings::Update::Unsolicited);
                if let Some(state) = state {
                    settings.receive_voice(
                        Some(1),
                        1,
                        kumochi_protocol::message::VoiceView {
                            speech_input: state,
                            spoken_replies: PartState::Ready,
                        },
                    );
                }
            });
            owner.error = Some("existing error".into());
            owner.refusal = Some(Text::TurnBusy);
            owner.chat_notice.present(
                ErrorKey::SettingsRefused.into(),
                crate::chat_notice::Source::Settings(None),
            );
            owner.voice.start = Some(Box::new(move |_, _| {
                attempts.set(attempts.get() + 1);
                Err(MicError::Unavailable)
            }));
            assert!(!owner.escape_hold.shows_stop());
        });
        let deliver = native(&owner, cx);
        let at = Instant::now();
        cx.update(|_| {
            chord(&deliver, Edge::Down, at);
            deliver(
                Event::Deadline,
                EventTime::new(at + Duration::from_millis(300)),
            );
        });
        cx.run_until_parked();
        owner.read_with(cx, |owner, _| {
            assert!(
                !owner.escape_hold.shows_stop(),
                "before status: {:?}, voice={}, fixture={}",
                owner.activity,
                owner.voice_working(),
                owner.escape_fixture
            );
        });
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        owner.read_with(cx, |owner, cx| {
            assert!(owner.text_open());
            assert_eq!(
                owner
                    .text
                    .as_ref()
                    .unwrap()
                    .input
                    .read(cx)
                    .value()
                    .to_string(),
                "unsent draft"
            );
            assert_eq!(owner.runs.kept(), [17]);
            assert_eq!(
                owner.error.as_ref().map(AsRef::as_ref),
                Some("existing error")
            );
            assert_eq!(
                owner.chat_notice.shown().map(|notice| notice.error.key()),
                Some(ErrorKey::SettingsRefused)
            );
            assert_eq!(owner.refusal, Some(Text::TurnBusy));
            assert_eq!(owner.voice.status, Some(notice));
            assert!(owner.voice.pending.is_none());
            assert!(
                !owner.escape_hold.shows_stop(),
                "after status: {:?}, voice={}, fixture={}",
                owner.activity,
                owner.voice_working(),
                owner.escape_fixture
            );
        });
        assert_eq!(starts.get(), 0);
        assert!(data.is_empty());
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        );
        assert!(controls.is_empty());
    }
}

#[gpui::test]
fn hold_to_talk_off_keeps_typing_and_never_warms_or_opens_a_source(cx: &mut TestAppContext) {
    let (owner, data, controls) = ready(cx);
    let starts = Rc::new(Cell::new(0));
    let attempts = Rc::clone(&starts);
    owner.update(cx, |owner, cx| {
        owner.text.as_mut().unwrap().typing.open();
        owner.settings.update(cx, |settings, _| {
            let mut saved = settings.view().unwrap().clone();
            saved.hold_to_talk = false;
            settings.set_view(saved, crate::settings::Update::Unsolicited);
        });
        owner.voice.start = Some(Box::new(move |_, _| {
            attempts.set(attempts.get() + 1);
            Err(MicError::Unavailable)
        }));
    });
    let deliver = native(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        deliver(
            Event::Deadline,
            EventTime::new(at + Duration::from_millis(300)),
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.text_open());
        assert_eq!(owner.voice.status, Some(Text::HoldDisabled));
        assert!(owner.voice.pending.is_none());
        assert!(!owner.link.input_is(GestureId(1)));
    });
    assert_eq!(starts.get(), 0);
    assert!(data.is_empty());
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1)
        }
    );
    assert!(controls.is_empty());
}

#[gpui::test]
fn stale_deferred_typing_cannot_take_over_a_new_native_hold(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        chord(&deliver, Edge::Up, at + Duration::from_millis(10));
        chord(&deliver, Edge::Down, at + Duration::from_millis(20));
        chord(&deliver, Edge::Up, at + Duration::from_millis(30));
        chord(&deliver, Edge::Down, at + Duration::from_millis(40));
        deliver(
            Event::Deadline,
            EventTime::new(at + Duration::from_millis(340)),
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.link.input_is(GestureId(3)));
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert!(!owner.text_open());
        assert_eq!(
            owner
                .voice
                .pending
                .as_ref()
                .unwrap()
                .token
                .mode(&owner.session.as_ref().unwrap().alive(), Instant::now()),
            Ok(Mode::Preparing)
        );
    });
}

#[gpui::test]
fn stale_deferred_escape_cannot_cancel_a_new_native_hold(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        deliver(
            Event::Deadline,
            EventTime::new(at + Duration::from_millis(300)),
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        owner
            .voice
            .pending
            .as_ref()
            .unwrap()
            .token
            .discard_unstarted()
            .unwrap();
        assert!(owner.link.send_control(
            owner.session.as_ref().unwrap(),
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        ));
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
        assert!(!owner.link.voice_stop_pending());
    });
    cx.update(|_| {
        deliver(
            Event::Key {
                key: Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                edge: Edge::Down,
            },
            EventTime::new(at + Duration::from_millis(310)),
        );
        deliver(
            Event::Escape,
            EventTime::new(at + Duration::from_millis(311)),
        );
        deliver(
            Event::Key {
                key: Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                edge: Edge::Up,
            },
            EventTime::new(at + Duration::from_millis(312)),
        );
        chord(&deliver, Edge::Up, at + Duration::from_millis(320));
        chord(&deliver, Edge::Down, at + Duration::from_millis(330));
        deliver(
            Event::Deadline,
            EventTime::new(at + Duration::from_millis(630)),
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.link.input_is(GestureId(2)));
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert_eq!(
            owner.voice.pending.as_ref().unwrap().token.gesture(),
            GestureId(2)
        );
    });
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture selects loss alone or a replacement session"
)]
fn escape_after_session_loss(reconnect: bool, cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    let finish = listen(&owner, &deliver, 1, at, cx);
    release(
        &owner,
        &deliver,
        &finish,
        at + Duration::from_millis(900),
        cx,
    );
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::RunState {
                    generation: 1,
                    run: RunId(1),
                    thread: ThreadId(1),
                    phase: Phase::Thinking,
                    error: None,
                    origin: Some(RunOrigin::Hold {
                        gesture: GestureId(1),
                    }),
                }]),
                cx,
            )
            .unwrap();
        assert!(owner.text_open());
    });
    cx.update(|_| {
        deliver(Event::Escape, after(at, 2000));
    });
    owner.update(cx, |owner, cx| {
        assert!(!owner.link.input_is(GestureId(1)));
        assert!(owner.voice.pending.is_some(), "the captured press waits");
        owner
            .link
            .set_recorded_generation(if reconnect { 2 } else { 0 });
        let mut events = vec![LinkEvent::State(crate::link::LinkState::Reconnecting)];
        if reconnect {
            events.extend([
                LinkEvent::Connected(Session::detached(
                    2,
                    kumochi_protocol::message::Incarnation(1),
                )),
                LinkEvent::RunState {
                    generation: 2,
                    run: RunId(8),
                    thread: ThreadId(1),
                    phase: Phase::Answering,
                    error: None,
                    origin: None,
                },
            ]);
        }
        owner.link_events(Batch::Plain(events), cx).unwrap();
        assert!(owner.voice.ended.is_none(), "loss forgot the old authority");
        assert!(owner.text_open());
    });
    while controls.try_recv().is_ok() {}
    cx.run_until_parked();
    assert_eq!(
        std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>(),
        [],
        "the stale press sends nothing on the replacement session"
    );
    owner.read_with(cx, |owner, _| {
        assert!(
            owner.text_open(),
            "the stale press did not dismiss the panel"
        );
        let run = reconnect.then_some(RunId(8));
        assert_eq!(owner.runs.stoppable(), run);
    });
    deliver(Event::Escape, after(at, 2100));
    cx.run_until_parked();
    let expected = if reconnect {
        vec![ControlToBrain::Stop {
            run: RunId(8),
            revealed: Vec::new(),
        }]
    } else {
        Vec::new()
    };
    assert_eq!(
        std::iter::from_fn(|| controls.try_recv().ok()).collect::<Vec<_>>(),
        expected,
        "a fresh unassociated press retains ordinary Esc"
    );
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), None);
        assert_eq!(owner.text_open(), reconnect);
    });
}

#[gpui::test]
fn a_stale_named_deferred_escape_cannot_dismiss_after_loss(cx: &mut TestAppContext) {
    escape_after_session_loss(false, cx);
}

#[gpui::test]
fn a_stale_named_deferred_escape_cannot_stop_a_replacement_session(cx: &mut TestAppContext) {
    escape_after_session_loss(true, cx);
}

#[gpui::test]
fn a_stale_named_deferred_escape_cannot_apply_to_a_dead_session(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    hold(&owner, 1, cx);
    let at = Instant::now();
    cx.update(|_| {
        deliver(Event::Escape, after(at, 0));
    });
    owner.update(cx, |owner, _| {
        owner
            .session
            .as_ref()
            .unwrap()
            .alive()
            .store(false, Ordering::Release);
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(
            owner.voice.pending.is_some(),
            "no stale owner intent was applied"
        );
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
    });
}

#[gpui::test]
fn stale_deferred_escape_marker_ends_on_raw_key_up_without_a_hotkey(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        deliver(
            Event::Deadline,
            EventTime::new(at + Duration::from_millis(300)),
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        owner
            .voice
            .pending
            .as_ref()
            .unwrap()
            .token
            .discard_unstarted()
            .unwrap();
    });
    cx.update(|_| {
        deliver(
            Event::Key {
                key: Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                edge: Edge::Down,
            },
            EventTime::new(at + Duration::from_millis(310)),
        );
        deliver(
            Event::Key {
                key: Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                edge: Edge::Up,
            },
            EventTime::new(at + Duration::from_millis(312)),
        );
        chord(&deliver, Edge::Up, at + Duration::from_millis(320));
    });
    cx.run_until_parked();
    owner.update(cx, |owner, cx| {
        owner.runs.submit("typed", |_| true).unwrap();
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::RunState {
                    generation: 1,
                    origin: Some(RunOrigin::Typed { turn: 1 }),
                    run: RunId(1),
                    thread: ThreadId(1),
                    phase: Phase::Thinking,
                    error: None,
                }]),
                cx,
            )
            .unwrap();
        assert!(owner.runs.working());
    });
    cx.update(|_| {
        deliver(
            Event::Escape,
            EventTime::new(at + Duration::from_millis(640)),
        );
    });
    cx.run_until_parked();
    owner.update(cx, |owner, _| {
        assert!(!owner.runs.working());
    });
}

pub(in crate::presence) fn until(
    cx: &mut TestAppContext,
    ready: impl Fn(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "fake source must progress");
        thread::sleep(Duration::from_millis(1));
    }
}

pub(in crate::presence) fn source(
    owner: &Entity<Presence>,
    cx: &mut TestAppContext,
    failure: Option<MicError>,
) -> (Sender<()>, Sender<()>, Receiver<()>, Receiver<()>) {
    cx.background_executor.allow_parking();
    let (open, opening) = mpsc::channel();
    let (finish, finishing) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    let (copied, samples) = mpsc::channel();
    owner.update(cx, |owner, _| {
        owner.voice.start = Some(Box::new(move |generation, session| {
            MicOwner::start_reserved_with_for_tests(
                generation,
                session,
                move |token, session, running, pcm| {
                    entered.send(()).unwrap();
                    opening.recv_timeout(Duration::from_secs(3)).unwrap();
                    let prepared = token.running_for_tests();
                    let mut collected = false;
                    if prepared.is_ok() {
                        running
                            .try_send(Running::for_tests(token.gesture(), 16_000))
                            .unwrap();
                        while token.mode(session, Instant::now()) == Ok(Mode::Preparing) {
                            thread::park_timeout(Duration::from_millis(1));
                        }
                        if token.mode(session, Instant::now()) == Ok(Mode::Collecting) {
                            collected = true;
                            // A test that dropped its receiver doesn't count copies.
                            let _counted = copied.send(());
                        }
                    }
                    finishing.recv_timeout(Duration::from_secs(3)).unwrap();
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    match token.mode(session, Instant::now()) {
                        Ok(Mode::Release) if collected => {
                            pcm.try_send(MicPcm {
                                gesture: token.gesture(),
                                seq: 0,
                                rate: 16_000,
                                last: true,
                                samples: vec![173, -19],
                            })
                            .unwrap();
                            Ok(Finalization::Released { packets: 1 })
                        }
                        Err(error) => Ok(Finalization::Discarded(error)),
                        _ => panic!("fake source must be stopped before finalization"),
                    }
                },
            )
        }));
    });
    (open, finish, started, samples)
}

#[cfg(windows)]
#[gpui::test]
fn queued_presence_claims_silent_output_and_cancels_the_fake_microphone(cx: &mut TestAppContext) {
    use crate::link::fixture;
    use kumochi_protocol::message::PartState;

    let _device = fixture::serialize_playback();
    let (id, hello, probe, brain) = kumochi_brain::ipc::offline_hold_peer();
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events);
    let owner = crate::presence::test_fixture::presence(cx);
    owner.update(cx, |owner, _| owner.link = pipe.handle.clone());
    let apply = |event, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
        });
    };
    let session = fixture::connected(fixture::next(&events));
    apply(LinkEvent::Connected(session.clone()), cx);
    loop {
        let event = fixture::next(&events);
        let voice_ready = matches!(
            &event,
            LinkEvent::Voice {
                view: kumochi_protocol::message::VoiceView {
                    speech_input: PartState::Ready,
                    ..
                },
                ..
            }
        );
        apply(event, cx);
        if voice_ready {
            break;
        }
    }
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    until(cx, |_| pipe.handle.admits_voice(session.generation()));

    let (open, finish, started, samples) = source(&owner, cx, None);
    let (token, stamp) = hold(&owner, 1, cx);
    loop {
        let event = fixture::next(&events);
        let admitted = matches!(
            &event,
            LinkEvent::VoiceHold {
                gesture: GestureId(1),
                status: HoldStatus::Admitted,
                ..
            }
        );
        assert!(!matches!(
            &event,
            LinkEvent::CaptureDone { .. } | LinkEvent::RunState { .. }
        ));
        apply(event, cx);
        if admitted {
            break;
        }
    }
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldListening)
        })
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();
    owner.update(cx, |owner, cx| {
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: true,
            },
            stamp,
            19,
            None,
            TalkHeld::default(),
            cx,
        );
    });
    finish.send(()).unwrap();
    until(cx, |cx| {
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(
                &event,
                LinkEvent::CaptureDone { .. } | LinkEvent::RunState { .. }
            ));
            apply(event, cx);
        }
        token.retired()
            && !pipe.handle.voice_stop_pending()
            && pipe.handle.admits_text(Some(&session))
    });
    assert_eq!(probe.decoded(), 0);
    pipe.handle.end(&session);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
    drop(owner);
    cx.run_until_parked();
}

#[cfg(all(windows, debug_assertions))]
fn spoken_run(
    owner: &Entity<Presence>,
    pipe: &crate::link::fixture::Authenticated,
    events: &Receiver<LinkEvent>,
    probe: &kumochi_brain::ipc::AudioProbe,
    origin: kumochi_protocol::message::RunOrigin,
    cx: &mut TestAppContext,
) -> RunId {
    use kumochi_protocol::message::{DataToShell, Phase};
    use std::time::Instant;

    let (mut named, mut done, mut ended) = (None, None, None);
    let (mut playback, mut rendered) = (None, false);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        assert!(
            Instant::now() < deadline,
            "the {origin:?} run finishes: named={named:?}, done={done:?}, ended={ended:?}, rendered={rendered}"
        );
        for event in events.try_iter() {
            match &event {
                LinkEvent::RunState {
                    run,
                    phase: Phase::Thinking,
                    origin: Some(seen),
                    ..
                } => {
                    assert_eq!(*seen, origin);
                    named = Some(*run);
                }
                LinkEvent::RunState {
                    run,
                    phase: Phase::Done,
                    ..
                } => done = Some(*run),
                LinkEvent::Data {
                    message: DataToShell::RunEnd { run },
                    ..
                } => ended = Some(*run),
                _ => {}
            }
            owner.update(cx, |owner, cx| {
                owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
            });
        }
        if let Some(run) = named
            && done == Some(run)
            && ended == Some(run)
            && probe.quiet() == run
        {
            assert!(rendered, "the {origin:?} reply reached the fake callback");
            return run;
        }
        if playback.is_none() {
            playback = pipe.playback.try_recv().ok();
        }
        if let Some(playback) = &mut playback
            && (playback.ready() || playback.stopped())
        {
            let mut samples = [0.0; 48];
            playback.render(&mut samples);
            rendered |= samples.contains(&(15_430.0_f32 / 32_768.0));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(all(windows, debug_assertions))]
#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one authenticated session from a spoken reply through the next held one"
)]
fn a_speaking_hold_releases_only_after_its_own_inhibit_retires(cx: &mut TestAppContext) {
    use crate::link::fixture;
    use kumochi_protocol::message::{PartState, RunOrigin};

    let _device = fixture::serialize_playback();
    let (id, hello, probe, brain) =
        kumochi_brain::ipc::offline_speaking_hold_peer("0 text Distinct sound. End.".into(), 8);
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events.clone());
    let owner = crate::presence::test_fixture::presence(cx);
    owner.update(cx, |owner, _| owner.link = pipe.handle.clone());
    let apply = |event, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
        });
    };
    let session = fixture::connected(fixture::next(&events));
    apply(LinkEvent::Connected(session.clone()), cx);
    loop {
        let event = fixture::next(&events);
        let voice_ready = matches!(
            &event,
            LinkEvent::Voice {
                view: kumochi_protocol::message::VoiceView {
                    speech_input: PartState::Ready,
                    ..
                },
                ..
            }
        );
        apply(event, cx);
        if voice_ready {
            break;
        }
    }
    until(cx, |_| pipe.handle.admits_text(Some(&session)));
    super::super::run_fence_tests::submit_draft(&owner, "the spoken turn", cx);
    let spoken = spoken_run(
        &owner,
        &pipe,
        &events,
        &probe,
        RunOrigin::Typed { turn: 1 },
        cx,
    );
    fixture::await_fake_disposal();
    fixture::prime_next_playback(&pipe.handle);
    until(cx, |_| pipe.handle.admits_voice(session.generation()));

    let (open, finish, started, samples) = source(&owner, cx, None);
    let (token, stamp) = hold(&owner, 1, cx);
    loop {
        let event = fixture::next(&events);
        let admitted = matches!(
            &event,
            LinkEvent::VoiceHold {
                gesture: GestureId(1),
                status: HoldStatus::Admitted,
                ..
            }
        );
        apply(event, cx);
        if admitted {
            break;
        }
    }
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldListening)
        })
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();
    let barrier = owner.read_with(cx, |owner, _| owner.voice.gates.hold_back());
    owner.update(cx, |owner, cx| {
        owner.link.microphone_gate().release(GestureId(1));
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: false,
            },
            stamp,
            443,
            None,
            TalkHeld::default(),
            cx,
        );
    });
    finish.send(()).unwrap();
    until(cx, |cx| {
        while let Ok(event) = events.try_recv() {
            apply(event, cx);
        }
        token.retired()
            && probe.received_mic() == 1
            && owner.read_with(cx, |owner, _| {
                owner
                    .voice
                    .pending
                    .as_ref()
                    .is_some_and(|hold| matches!(hold.source, Source::Joined(1)))
            })
    });
    owner.read_with(cx, |owner, _| {
        let hold = owner.voice.pending.as_ref().unwrap();
        assert!(
            hold.unretired && matches!(hold.source, Source::Joined(1)),
            "a retired microphone alone doesn't release a speaking hold"
        );
    });
    assert_eq!(probe.decoded(), 0);

    drop(barrier);
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner
                .voice
                .pending
                .as_ref()
                .is_some_and(|hold| matches!(hold.source, Source::Sent))
        })
    });
    let held = spoken_run(
        &owner,
        &pipe,
        &events,
        &probe,
        RunOrigin::Hold {
            gesture: GestureId(1),
        },
        cx,
    );
    assert!(held > spoken);
    assert_eq!(probe.decoded(), 1);
    pipe.handle.end(&session);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
    drop(owner);
    cx.run_until_parked();
}

#[gpui::test]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture drives the source-to-run handoff in both delivery orders"
)]
fn running_and_true_receipt_rendezvous_with_release_in_either_ui_order(cx: &mut TestAppContext) {
    for receipt_first in [false, true] {
        let (owner, data, controls) = ready(cx);
        let (open, finish, started, samples) = source(&owner, cx, None);
        let (token, stamp) = hold(&owner, 1, cx);
        assert_eq!(started.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                thread: Some(ThreadId(1)),
                selection: 1,
            }
        );
        status(&owner, 2, 1, HoldStatus::Admitted, cx);
        status(&owner, 1, 1, HoldStatus::Loading, cx);
        assert_eq!(started.try_recv(), Err(mpsc::TryRecvError::Empty));
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.voice.status, Some(Text::VoiceLoading));
            assert!(!owner.dock.read(cx).listening);
        });
        open.send(()).unwrap();
        until(cx, |cx| {
            owner.read_with(cx, |o, _| o.voice.status == Some(Text::HoldListening))
        });
        samples.recv_timeout(Duration::from_secs(3)).unwrap();
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        owner.read_with(cx, |owner, cx| assert!(owner.dock.read(cx).listening));
        owner.update(cx, |owner, _| {
            owner.link.microphone_gate().release(GestureId(1));
        });
        if receipt_first {
            finish.send(()).unwrap();
            until(cx, |cx| {
                owner.read_with(cx, |o, _| {
                    o.voice
                        .pending
                        .as_ref()
                        .is_some_and(|h| matches!(h.source, Source::Joined(1)))
                })
            });
            owner.read_with(cx, |owner, _| {
                assert!(matches!(
                    owner.voice.pending.as_ref().unwrap().source,
                    Source::Joined(1)
                ));
            });
        }
        owner.update(cx, |owner, cx| {
            owner.voice_intent(
                Intent::End {
                    gesture: GestureId(1),
                    cancelled: false,
                },
                stamp,
                443,
                None,
                TalkHeld::default(),
                cx,
            );
            assert_eq!(owner.voice.status, Some(Text::HoldStopping));
        });
        if !receipt_first {
            assert!(controls.is_empty());
            finish.send(()).unwrap();
            until(cx, |cx| {
                owner.read_with(cx, |o, _| {
                    o.voice
                        .pending
                        .as_ref()
                        .is_some_and(|h| matches!(h.source, Source::Sent))
                })
            });
        }
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        );
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::KeyUp {
                gesture: GestureId(1),
                t_qpc: 443,
                chunks: 1
            }
        );
        assert!(controls.is_empty());
        assert_eq!(
            data.try_recv().unwrap(),
            DataToBrain::MicPcm(MicPcm {
                gesture: GestureId(1),
                seq: 0,
                rate: 16_000,
                last: true,
                samples: vec![173, -19],
            })
        );
        assert!(data.is_empty());
        assert!(token.retired());
        owner.read_with(cx, |owner, _| {
            assert!(matches!(
                owner.voice.pending.as_ref().unwrap().source,
                Source::Sent
            ));
        });
        status(&owner, 1, 1, HoldStatus::Transcribing, cx);
        owner.update(cx, |owner, cx| {
            assert_eq!(owner.voice.status, Some(Text::HoldTranscribing));
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::RunState {
                        generation: 1,
                        origin: Some(RunOrigin::Hold {
                            gesture: GestureId(1),
                        }),
                        run: RunId(1),
                        thread: ThreadId(1),
                        phase: Phase::Thinking,
                        error: None,
                    }]),
                    cx,
                )
                .unwrap();
            assert_eq!(owner.voice.status, Some(Text::HoldThinking));
            assert!(owner.text_open());
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::RunState {
                        generation: 1,
                        origin: Some(RunOrigin::Hold {
                            gesture: GestureId(1),
                        }),
                        run: RunId(1),
                        thread: ThreadId(1),
                        phase: Phase::Answering,
                        error: None,
                    }]),
                    cx,
                )
                .unwrap();
            assert!(owner.voice.pending.is_none());
            assert_eq!(owner.voice.status, None);
            assert!(!owner.link.input_is(GestureId(1)));
            assert!(!owner.dock.read(cx).listening);
        });
    }
}

#[gpui::test]
fn release_during_open_never_claims_listening_or_stop_before_join(cx: &mut TestAppContext) {
    let (owner, data, controls) = ready(cx);
    let (open, finish, started, _samples) = source(&owner, cx, None);
    let (token, stamp) = hold(&owner, 1, cx);
    controls.try_recv().unwrap();
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    owner.update(cx, |owner, cx| {
        owner.link.microphone_gate().release(GestureId(1));
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: false,
            },
            stamp,
            9,
            None,
            TalkHeld::default(),
            cx,
        );
        assert_eq!(owner.voice.status, Some(Text::HoldStopping));
        assert!(!owner.dock.read(cx).listening);
        assert!(!token.retired());
    });
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::GestureCancel {
            gesture: GestureId(1)
        }
    );
    assert!(controls.is_empty());
    open.send(()).unwrap();
    cx.run_until_parked();
    assert!(!token.retired());
    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.pending.is_none())
    });
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
        assert!(!owner.dock.read(cx).listening);
    });
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1)
        }
    );
    assert!(controls.is_empty());
    assert!(data.is_empty());
    assert!(token.retired());
}

#[gpui::test]
fn first_terminal_is_preserved_and_unconfirmed_cleanup_cannot_release_debt(
    cx: &mut TestAppContext,
) {
    let (owner, data, controls) = ready(cx);
    let (open, finish, started, samples) = source(&owner, cx, Some(MicError::Unconfirmed));
    let (token, _) = hold(&owner, 1, cx);
    controls.try_recv().unwrap();
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.status == Some(Text::HoldListening))
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    status(&owner, 1, 1, HoldStatus::Empty, cx);
    owner.update(cx, |owner, cx| {
        owner.cancel_voice(cx);
        assert_eq!(
            owner.voice.pending.as_ref().unwrap().terminal,
            Some(Text::TurnBusy)
        );
        assert_eq!(owner.voice.status, Some(Text::HoldStopping));
        assert!(!owner.voice_working());
    });
    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.status == Some(Text::HoldUnavailable))
    });
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.voice.pending.as_ref().unwrap().terminal,
            Some(Text::TurnBusy)
        );
        assert_eq!(
            owner.link.microphone_gate().arm(GestureId(2)).unwrap_err(),
            MicError::Stale
        );
        assert!(!owner.dock.read(cx).listening);
        assert!(!token.retired());
    });
    assert_eq!(
        controls.try_recv().unwrap(),
        ControlToBrain::GestureCancel {
            gesture: GestureId(1)
        }
    );
    assert!(controls.is_empty());
    assert!(data.is_empty());
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldUnavailable));
    });
    // The keys come up late, normally: the warning stays and nothing is released.
    owner.update(cx, |owner, cx| {
        let stamp = owner.voice.pending.as_ref().unwrap().stamp;
        owner.voice_intent(
            Intent::End {
                gesture: GestureId(1),
                cancelled: false,
            },
            stamp,
            0,
            Some(kumochi_protocol::geometry::PhysicalPoint([900, 600])),
            crate::input::TalkHeld::default(),
            cx,
        );
    });
    cx.run_until_parked();
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldUnavailable));
        assert_eq!(owner.voice.pending.as_ref().unwrap().released, None);
        assert!(owner.voice.watched.is_none());
    });
    assert!(controls.is_empty());
    assert!(data.is_empty());
}

#[gpui::test]
fn a_stalled_open_or_stop_warns_without_releasing_its_generation(cx: &mut TestAppContext) {
    for opening in [true, false] {
        let (owner, data, controls) = ready(cx);
        let (open, finish, started, samples) = source(&owner, cx, None);
        let (token, stamp) = hold(&owner, 1, cx);
        controls.try_recv().unwrap();
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        cx.run_until_parked();
        if opening {
            cx.background_executor
                .advance_clock(Duration::from_secs(11));
        } else {
            open.send(()).unwrap();
            until(cx, |cx| {
                owner.read_with(cx, |o, _| o.voice.status == Some(Text::HoldListening))
            });
            samples.recv_timeout(Duration::from_secs(3)).unwrap();
            owner.update(cx, |owner, cx| {
                owner.link.microphone_gate().release(GestureId(1));
                owner.voice_intent(
                    Intent::End {
                        gesture: GestureId(1),
                        cancelled: false,
                    },
                    stamp,
                    19,
                    None,
                    TalkHeld::default(),
                    cx,
                );
            });
            cx.background_executor
                .advance_clock(Duration::from_millis(30));
        }
        cx.run_until_parked();
        cx.background_executor.advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.voice.status, Some(Text::HoldUnavailable));
            assert!(matches!(
                owner.voice.pending.as_ref().unwrap().source,
                Source::Recording
            ));
            assert!(!owner.dock.read(cx).listening);
            assert!(!token.retired());
            assert_eq!(
                owner.link.microphone_gate().arm(GestureId(2)).unwrap_err(),
                MicError::Stale
            );
        });
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::GestureCancel {
                gesture: GestureId(1)
            }
        );
        assert!(controls.is_empty());
        assert!(data.is_empty());
        if opening {
            open.send(()).unwrap();
        }
        finish.send(()).unwrap();
        until(cx, |cx| {
            owner.read_with(cx, |o, _| o.voice.pending.is_none())
        });
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
            assert!(!owner.dock.read(cx).listening);
        });
        assert!(token.retired());
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        );
        assert!(controls.is_empty());
        assert!(data.is_empty());
    }
}

/// The brain acknowledging the stop receipt of `gesture`, which the next hold waits for.
fn accepted(owner: &Entity<Presence>, gesture: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, _| {
        owner
            .link
            .accept_recorded_microphone_stop(1, GestureId(gesture));
    });
}

pub(super) fn after(at: Instant, millis: u64) -> EventTime {
    EventTime::new(at + Duration::from_millis(millis))
}

pub(super) fn listen(
    owner: &Entity<Presence>,
    deliver: &Deliver,
    gesture: u32,
    at: Instant,
    cx: &mut TestAppContext,
) -> Sender<()> {
    let (open, finish, started, samples) = source(owner, cx, None);
    chord(deliver, Edge::Down, at);
    deliver(Event::Deadline, after(at, 301));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
    });
    status(owner, 1, gesture, HoldStatus::Admitted, cx);
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    open.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.status == Some(Text::HoldListening))
    });
    samples.recv_timeout(Duration::from_secs(3)).unwrap();
    finish
}

fn release(
    owner: &Entity<Presence>,
    deliver: &Deliver,
    finish: &Sender<()>,
    at: Instant,
    cx: &mut TestAppContext,
) {
    chord(deliver, Edge::Up, at);
    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| {
            o.voice
                .pending
                .as_ref()
                .is_some_and(|h| matches!(h.source, Source::Sent))
        })
    });
}

#[gpui::test]
fn esc_after_a_finished_hold_run_leaves_voice_alone_and_the_next_hold_listens(
    cx: &mut TestAppContext,
) {
    let (owner, _data, controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    let finish = listen(&owner, &deliver, 1, at, cx);
    release(
        &owner,
        &deliver,
        &finish,
        at + Duration::from_millis(900),
        cx,
    );
    owner.update(cx, |owner, cx| {
        for (phase, origin) in [
            (
                Phase::Thinking,
                Some(RunOrigin::Hold {
                    gesture: GestureId(1),
                }),
            ),
            (Phase::Answering, None),
            (Phase::Done, None),
        ] {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::RunState {
                        generation: 1,
                        run: RunId(1),
                        thread: ThreadId(1),
                        phase,
                        error: None,
                        origin,
                    }]),
                    cx,
                )
                .unwrap();
        }
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, None);
        owner.presenter =
            crate::presenter::tests::painter_fixture(Box::new(crate::presenter::tests::Charged(0)))
                .0;
        owner.presenter.frames().borrow_mut().shown = true;
        owner.runs_changed(cx).unwrap();
        assert!(owner.presenter.holds_scenes());
    });
    while controls.try_recv().is_ok() {}
    deliver(Event::Escape, after(at, 2_000));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, None);
        assert!(!owner.presenter.holds_scenes());
    });
    assert!(
        std::iter::from_fn(|| controls.try_recv().ok())
            .all(|control| !matches!(control, ControlToBrain::GestureCancel { .. }))
    );
    let at = at + Duration::from_secs(3);
    accepted(&owner, 1, cx);
    let finish = listen(&owner, &deliver, 2, at, cx);
    release(
        &owner,
        &deliver,
        &finish,
        at + Duration::from_millis(900),
        cx,
    );
}

#[gpui::test]
fn esc_after_a_busy_or_offline_refusal_clears_the_drawing_on_screen(cx: &mut TestAppContext) {
    for (busy, notice) in [(true, Text::TurnBusy), (false, Text::HoldFailed)] {
        let (owner, _data, _controls) = ready(cx);
        let deliver = native(&owner, cx);
        owner.update(cx, |owner, cx| {
            if busy {
                owner.runs.begin_hold(GestureId(1), None).unwrap();
                owner.runs.cancel_hold();
                owner.runs.take_hold_cancel().unwrap();
            } else {
                owner.runs.disconnected();
            }
            owner.presenter = crate::presenter::tests::painter_fixture(Box::new(
                crate::presenter::tests::Charged(0),
            ))
            .0;
            owner.presenter.frames().borrow_mut().shown = true;
            owner.runs_changed(cx).unwrap();
            assert!(owner.activity.drawing);
        });
        let at = Instant::now();
        chord(&deliver, Edge::Down, at);
        deliver(Event::Deadline, after(at, 301));
        chord(&deliver, Edge::Up, at + Duration::from_millis(400));
        cx.run_until_parked();
        owner.read_with(cx, |owner, _| {
            assert!(owner.voice.pending.is_none());
            assert_eq!(owner.voice.status, Some(notice));
            assert!(!owner.link.input_is(GestureId(1)));
            assert!(owner.activity.drawing);
            assert!(owner.presenter.holds_scenes());
        });
        deliver(Event::Escape, after(at, 1_000));
        cx.run_until_parked();
        owner.read_with(cx, |owner, _| {
            assert!(!owner.presenter.holds_scenes());
            assert!(owner.voice.pending.is_none());
        });
    }
}

#[gpui::test]
fn a_run_that_fails_or_stops_after_thinking_ends_the_hold_quietly(cx: &mut TestAppContext) {
    for phase in [Phase::Failed, Phase::Interrupted] {
        let (owner, _data, controls) = ready(cx);
        let deliver = native(&owner, cx);
        let at = Instant::now();
        let finish = listen(&owner, &deliver, 1, at, cx);
        release(
            &owner,
            &deliver,
            &finish,
            at + Duration::from_millis(900),
            cx,
        );
        owner.update(cx, |owner, cx| {
            for (phase, origin) in [
                (
                    Phase::Thinking,
                    Some(RunOrigin::Hold {
                        gesture: GestureId(1),
                    }),
                ),
                (phase, None),
            ] {
                owner
                    .link_events(
                        Batch::Plain(vec![LinkEvent::RunState {
                            generation: 1,
                            run: RunId(1),
                            thread: ThreadId(1),
                            phase,
                            error: None,
                            origin,
                        }]),
                        cx,
                    )
                    .unwrap();
            }
            assert!(owner.voice.pending.is_none());
            assert_eq!(owner.voice.status, None);
            assert!(!owner.link.input_is(GestureId(1)));
        });
        assert!(
            std::iter::from_fn(|| controls.try_recv().ok())
                .all(|control| !matches!(control, ControlToBrain::GestureCancel { .. }))
        );
        let at = at + Duration::from_secs(3);
        accepted(&owner, 1, cx);
        let finish = listen(&owner, &deliver, 2, at, cx);
        release(
            &owner,
            &deliver,
            &finish,
            at + Duration::from_millis(900),
            cx,
        );
    }
}

#[gpui::test]
fn esc_during_listening_cancels_that_hold_and_the_next_hold_listens(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    let finish = listen(&owner, &deliver, 1, at, cx);
    deliver(Event::Escape, after(at, 600));
    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.pending.is_none())
    });
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
    });
    chord(&deliver, Edge::Up, at + Duration::from_millis(900));
    let at = at + Duration::from_secs(2);
    accepted(&owner, 1, cx);
    let finish = listen(&owner, &deliver, 2, at, cx);
    release(
        &owner,
        &deliver,
        &finish,
        at + Duration::from_millis(900),
        cx,
    );
}

#[gpui::test]
fn esc_with_the_talk_keys_is_asked_for_only_while_a_hold_s_keys_are_down(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let asked = |cx: &mut TestAppContext| owner.read_with(cx, |owner, _| owner.talk_escape.held());
    assert!(!asked(cx), "no hold yet");
    let at = Instant::now();
    let finish = listen(&owner, &deliver, 1, at, cx);
    assert!(asked(cx), "the keys are down");
    for edge in [Edge::Down, Edge::Up] {
        let key = Key::Escape(crate::input::PhysicalKey::new(1).unwrap());
        deliver(Event::Key { key, edge }, after(at, 600));
    }
    cx.run_until_parked();
    assert!(!asked(cx), "Esc ended the hold with its keys still down");
    finish.send(()).unwrap();
    until(cx, |cx| {
        owner.read_with(cx, |o, _| o.voice.pending.is_none())
    });
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
    });
    chord(&deliver, Edge::Up, at + Duration::from_millis(900));
    let at = at + Duration::from_secs(2);
    accepted(&owner, 1, cx);
    let finish = listen(&owner, &deliver, 2, at, cx);
    assert!(asked(cx), "the next hold asks again");
    release(
        &owner,
        &deliver,
        &finish,
        at + Duration::from_millis(900),
        cx,
    );
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice_working(), "the released hold still owns Esc");
        assert!(!owner.talk_escape.held(), "but its keys are up");
    });
}

#[gpui::test]
fn a_hold_seen_only_at_release_is_refused_with_a_notice_and_a_repeat_recovers_the_next(
    cx: &mut TestAppContext,
) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    chord(&deliver, Edge::Down, at);
    chord(&deliver, Edge::Up, at + Duration::from_millis(400));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });
    let at = at + Duration::from_secs(1);
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Key {
            key: Key::LeftAlt,
            edge: Edge::Down,
        },
        after(at, 500),
    );
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldPreparing));
        assert_eq!(
            owner
                .voice
                .pending
                .as_ref()
                .map(|hold| hold.token.gesture()),
            Some(GestureId(2))
        );
    });
}

#[gpui::test]
fn the_native_callback_reads_the_pointer_at_recognition_and_when_the_keys_come_up_and_logs_neither(
    cx: &mut TestAppContext,
) {
    use super::super::screen_hold_tests::{luna, on, screen_ready};
    use crate::platform::cursor_fixture;
    use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};

    const ORIGIN: [i32; 2] = [1_613, 977];
    const MOVED: [i32; 2] = [1_777, 1_013];
    const RELEASED: [i32; 2] = [1_783, 1_019];
    let (captured, _logging, _second) = capture();
    let ready = screen_ready(Some(on(&luna())), cx);
    let deliver = native(&ready.owner, cx);
    cursor_fixture::set(Some(ORIGIN));
    let at = Instant::now();
    chord(&deliver, Edge::Down, at);
    deliver(Event::Deadline, after(at, 301));
    cx.run_until_parked();
    ready.owner.update(cx, |owner, _| {
        owner.voice.pending.as_mut().expect("a hold").collected = true;
        owner.voice.status = Some(Text::HoldListening);
    });
    // The line starts where the callback read the pointer at recognition, and a frame reads
    // where it has gone.
    cursor_fixture::set(Some(MOVED));
    for window in cx.windows() {
        window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("an open window");
    }
    cx.run_until_parked();
    // The release names where the callback read it when the keys came up, and the line ends
    // there too.
    cursor_fixture::set(Some(RELEASED));
    chord(&deliver, Edge::Up, at + Duration::from_millis(400));
    cx.run_until_parked();
    let release =
        std::iter::from_fn(|| ready.controls.try_recv().ok()).find_map(|message| match message {
            ControlToBrain::ScreenRelease { pointer, path, .. } => Some((pointer, path)),
            _ => None,
        });
    assert_eq!(
        release,
        Some((
            ReleasePointer(PhysicalPoint(RELEASED)),
            PointerPath(vec![
                PhysicalPoint(ORIGIN),
                PhysicalPoint(MOVED),
                PhysicalPoint(RELEASED)
            ])
        ))
    );
    // Where the user pointed is theirs: no log line carries a place the pointer was.
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("held speech input"), "the hold was logged");
    for [x, y] in [ORIGIN, MOVED, RELEASED] {
        for place in [format!("{x}, {y}"), format!("{x},{y}"), format!("{x} {y}")] {
            assert!(!text.contains(&place), "a log line names a place");
        }
    }
}

#[cfg(windows)]
#[gpui::test]
fn superseding_native_hold_returns_before_outbox_release_and_sends_cancel_first(
    cx: &mut TestAppContext,
) {
    use crate::link::fixture;

    let (owner, _data, _controls) = ready(cx);
    let pipe = fixture::attended("superseding-hold");
    let events = fixture::patient(pipe.events.clone());
    let session = fixture::connected(fixture::next(&events));
    owner.update(cx, |this, _| {
        this.link = pipe.handle.clone();
        this.session = Some(session.clone());
    });
    let old = GestureId(1);
    let replacement = GestureId(2);
    let send = native(&owner, cx);
    let at = Instant::now();
    chord(&send, Edge::Down, at);
    cx.run_until_parked();
    assert!(matches!(
        pipe.next_control(),
        ControlToBrain::KeyDown { gesture, .. } if gesture == old
    ));
    chord(&send, Edge::Up, at + Duration::from_millis(100));

    pipe.handle.track_input(old);
    let at = at + Duration::from_secs(1);
    chord(&send, Edge::Down, at);
    cx.run_until_parked();
    assert!(matches!(
        pipe.next_control(),
        ControlToBrain::KeyDown { gesture, .. } if gesture == replacement
    ));

    fixture::with_outbox_held(&pipe.handle, &session, || {
        send(Event::Deadline, after(at, 301));
        assert!(!pipe.handle.input_is(old), "local revocation is immediate");
        assert!(
            pipe.handle.input_is(replacement),
            "replacement is already tracked"
        );
    });

    cx.run_until_parked();
    assert_eq!(
        pipe.next_control(),
        ControlToBrain::GestureCancel { gesture: old }
    );
    assert!(matches!(
        pipe.next_control(),
        ControlToBrain::VoiceHoldRecognized { gesture, .. } if gesture == replacement
    ));
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
enum CancelAt {
    RawEscape,
    HotkeyEscape,
    End,
    OpenInput,
    ClockFailure,
    ClosedQueue,
}

#[cfg(windows)]
impl CancelAt {
    fn deliver(self, send: &Deliver, now: Instant) {
        use crate::presence::gesture::{CLOSE_DELIVERY, FAIL_CLOCK};

        match self {
            Self::RawEscape => {
                send(
                    Event::Key {
                        key: Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                        edge: Edge::Down,
                    },
                    after(now, 320),
                );
            }
            Self::HotkeyEscape => {
                send(Event::Escape, after(now, 320));
            }
            Self::End => {
                send(Event::Reset, after(now, 320));
            }
            Self::OpenInput => {
                chord(send, Edge::Up, now + Duration::from_millis(30));
            }
            Self::ClockFailure => {
                FAIL_CLOCK.set(true);
                send(Event::Deadline, after(now, 320));
            }
            Self::ClosedQueue => {
                CLOSE_DELIVERY.set(true);
                let mut time = after(now, 10).with_keyboard_read(Some);
                time.defer_terminal = true;
                send(
                    Event::Key {
                        key: Key::LeftAlt,
                        edge: Edge::Down,
                    },
                    time,
                );
            }
        }
    }
}

#[cfg(windows)]
fn check_native_cancel(cx: &mut TestAppContext, at: CancelAt) {
    use crate::link::fixture;

    let (owner, _data, _controls) = ready(cx);
    let pipe = fixture::attended(&format!("native-cancel-{at:?}"));
    let events = fixture::patient(pipe.events.clone());
    let session = fixture::connected(fixture::next(&events));
    owner.update(cx, |this, _| {
        this.link = pipe.handle.clone();
        this.session = Some(session.clone());
    });
    let send = native(&owner, cx);
    let now = Instant::now();
    let (gesture, mic) = if matches!(at, CancelAt::End) {
        chord(&send, Edge::Down, now);
        send(Event::Deadline, after(now, 301));
        cx.run_until_parked();
        assert!(matches!(
            pipe.next_control(),
            ControlToBrain::KeyDown {
                gesture: GestureId(1),
                ..
            }
        ));
        assert!(matches!(
            pipe.next_control(),
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                ..
            }
        ));
        let token = owner.read_with(cx, |owner, _| {
            owner.voice.pending.as_ref().unwrap().token.clone()
        });
        (GestureId(1), token)
    } else {
        if matches!(at, CancelAt::OpenInput) {
            chord(&send, Edge::Down, now);
            chord(&send, Edge::Up, now + Duration::from_millis(10));
            chord(&send, Edge::Down, now + Duration::from_millis(20));
            cx.run_until_parked();
            for id in [1, 2] {
                assert!(
                    matches!(pipe.next_control(), ControlToBrain::KeyDown { gesture, .. } if gesture == GestureId(id))
                );
            }
        }
        if matches!(at, CancelAt::ClosedQueue) {
            send(
                Event::Key {
                    key: Key::LeftControl,
                    edge: Edge::Down,
                },
                EventTime::new(now),
            );
        }
        let gesture = GestureId(10);
        pipe.handle.track_input(gesture);
        let token = pipe.handle.microphone_gate().arm(gesture).unwrap();
        (gesture, token)
    };
    assert!(pipe.handle.input_is(gesture));
    assert_eq!(
        mic.mode(&session.alive(), Instant::now()),
        Ok(Mode::Preparing)
    );

    fixture::with_outbox_held(&pipe.handle, &session, || {
        at.deliver(&send, now);
        assert!(
            !pipe.handle.input_is(gesture),
            "the callback revokes the tracked gesture"
        );
        assert_eq!(
            mic.mode(&session.alive(), Instant::now()),
            Err(MicError::Cancelled)
        );
    });

    cx.run_until_parked();
    assert_eq!(
        pipe.next_control(),
        ControlToBrain::GestureCancel { gesture }
    );
    if matches!(at, CancelAt::OpenInput) {
        owner.read_with(cx, |owner, _| assert!(owner.text_open()));
    }
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_raw_escape(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::RawEscape);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_hotkey_escape(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::HotkeyEscape);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_cancelled_end(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::End);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_open_input(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::OpenInput);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_clock_failure(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::ClockFailure);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_on_closed_queue(cx: &mut TestAppContext) {
    check_native_cancel(cx, CancelAt::ClosedQueue);
}

#[cfg(windows)]
#[gpui::test]
fn native_cancel_never_waits_or_offers_a_replacement_after_a_stale_cancel(cx: &mut TestAppContext) {
    use crate::link::fixture;

    let (owner, _data, _controls) = ready(cx);
    let pipe = fixture::attended("native-replacement-stale");
    let events = fixture::patient(pipe.events.clone());
    let session = fixture::connected(fixture::next(&events));
    owner.update(cx, |this, _| {
        this.link = pipe.handle.clone();
        this.session = Some(session.clone());
    });
    let send = native(&owner, cx);
    let at = Instant::now();
    chord(&send, Edge::Down, at);
    cx.run_until_parked();
    assert!(matches!(
        pipe.next_control(),
        ControlToBrain::KeyDown {
            gesture: GestureId(1),
            ..
        }
    ));
    pipe.handle
        .track_input_on(GestureId(9), session.generation() - 1);

    fixture::with_outbox_held(&pipe.handle, &session, || {
        send(Event::Deadline, after(at, 301));
        assert!(pipe.handle.input_is(GestureId(1)));
    });
    cx.run_until_parked();

    assert!(pipe.handle.send_control(&session, ControlToBrain::Pong));
    assert_eq!(pipe.next_control(), ControlToBrain::Pong);
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert!(!owner.link.input_is(GestureId(1)));
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });
}

#[gpui::test]
fn native_review_plain_escape_cant_stop_a_turn_submitted_after_the_press(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let send = native(&owner, cx);

    send(Event::Escape, EventTime::new(Instant::now()));
    owner.update(cx, |owner, cx| {
        owner.runs.submit("typed after Esc", |_| true).unwrap();
        owner
            .link_events(
                Batch::Plain(vec![LinkEvent::RunState {
                    generation: 1,
                    origin: Some(RunOrigin::Typed { turn: 1 }),
                    run: RunId(1),
                    thread: ThreadId(1),
                    phase: Phase::Thinking,
                    error: None,
                }]),
                cx,
            )
            .unwrap();
    });
    cx.run_until_parked();

    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), Some(RunId(1)));
    });
    assert!(
        std::iter::from_fn(|| controls.try_recv().ok())
            .all(|message| !matches!(message, ControlToBrain::Stop { .. }))
    );
}

#[gpui::test]
fn native_review_full_control_queue_discards_the_unpublished_hold_locally(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let send = native(&owner, cx);
    let at = Instant::now();
    chord(&send, Edge::Down, at);
    let (controls, session) = owner.update(cx, |owner, _| {
        let controls = owner.link.record_controls_bounded();
        let session = owner.session.clone().expect("connected");
        while owner.link.send_control(&session, ControlToBrain::Pong) {}
        assert_eq!(Some(controls.len()), controls.capacity());
        (controls, session)
    });

    send(Event::Deadline, after(at, 301));
    assert!(
        session.open(),
        "an unpublished offer needs no wire cancel or disconnect"
    );
    owner.read_with(cx, |owner, _| {
        assert!(!owner.link.input_is(GestureId(1)));
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });
    assert!(
        std::iter::from_fn(|| controls.try_recv().ok())
            .all(|message| message == ControlToBrain::Pong)
    );
    cx.run_until_parked();
    assert!(
        controls.is_empty(),
        "no stop or cancel for the unpublished offer"
    );

    chord(&send, Edge::Up, at + Duration::from_millis(400));
    let next = at + Duration::from_secs(1);
    chord(&send, Edge::Down, next);
    send(Event::Deadline, after(next, 301));
    cx.run_until_parked();
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::KeyDown {
            gesture: GestureId(2),
            ..
        }
    ));
    assert!(matches!(
        controls.try_recv().unwrap(),
        ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(2),
            ..
        }
    ));
    assert!(controls.is_empty());
}

#[gpui::test]
fn a_fenced_first_state_ends_transcribing_and_releases_escape(cx: &mut TestAppContext) {
    super::super::test_fixture::without_desktop(cx);
    let (owner, _data, controls) = ready(cx);
    let _hold = hold(&owner, 1, cx);
    owner.update(cx, |owner, _| {
        owner.runs.apply_scenes(&[]).unwrap();
        let hold = owner.voice.pending.as_mut().unwrap();
        hold.source = Source::Joined(1);
        hold.released = Some(1);
        owner.finish_voice();
    });
    status(&owner, 1, 1, HoldStatus::Transcribing, cx);
    let state = |run, gesture, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::RunState {
                        generation: 1,
                        run: RunId(run),
                        thread: ThreadId(1),
                        phase: Phase::Thinking,
                        error: None,
                        origin: Some(RunOrigin::Hold {
                            gesture: GestureId(gesture),
                        }),
                    }]),
                    cx,
                )
                .unwrap();
        });
    };
    let take_stops_and_cancels = || {
        std::iter::from_fn(|| controls.try_recv().ok())
            .filter(|control| {
                matches!(
                    control,
                    ControlToBrain::Stop { .. } | ControlToBrain::GestureCancel { .. }
                )
            })
            .collect::<Vec<_>>()
    };

    state(5, 9, cx);
    assert_eq!(
        take_stops_and_cancels(),
        [ControlToBrain::Stop {
            run: RunId(5),
            revealed: Vec::new(),
        }]
    );
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldTranscribing));
        assert!(owner.escape_hold.shows_stop());
    });

    state(4, 1, cx);
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
        assert!(!owner.escape_hold.shows_stop());
    });
    assert_eq!(
        take_stops_and_cancels(),
        [ControlToBrain::GestureCancel {
            gesture: GestureId(1),
        }]
    );
}

/// One run's state, as the brain tells it.
fn run_state(
    owner: &Entity<Presence>,
    run: u64,
    phase: Phase,
    origin: Option<RunOrigin>,
    cx: &mut TestAppContext,
) {
    let event = LinkEvent::RunState {
        generation: 1,
        run: RunId(run),
        thread: ThreadId(1),
        phase,
        error: None,
        origin,
    };
    owner.update(cx, |owner, cx| {
        owner.link_events(Batch::Plain(vec![event]), cx).unwrap();
    });
}

/// Whether Esc is wanted system-wide: tests have no window to register it, so Stop shows.
fn esc_held(owner: &Entity<Presence>, cx: &mut TestAppContext) -> bool {
    owner.read_with(cx, |owner, _| owner.escape_hold.shows_stop())
}

/// Global Esc follows the hold itself, not the runs it folds into: a hold takes Esc beside a run
/// that already works.
#[gpui::test]
fn a_hold_takes_global_esc_beside_a_working_run(cx: &mut TestAppContext) {
    super::super::test_fixture::without_desktop(cx);
    let (owner, _data, _controls) = ready(cx);
    run_state(&owner, 3, Phase::Thinking, None, cx);
    assert!(
        !esc_held(&owner, cx),
        "a thinking run alone leaves Esc to other apps"
    );
    hold(&owner, 1, cx);
    assert!(
        esc_held(&owner, cx),
        "the hold takes Esc beside the working run"
    );
}

/// A hold gives global Esc back once its own run answers, though that run still works.
#[gpui::test]
fn a_hold_gives_global_esc_back_once_its_run_answers(cx: &mut TestAppContext) {
    super::super::test_fixture::without_desktop(cx);
    let (owner, _data, _controls) = ready(cx);
    // The brain described no scenes, so the answer's open panel shows no drawing Esc clears.
    owner.update(cx, |owner, _| {
        owner.runs.apply_scenes(&[]).unwrap();
    });
    hold(&owner, 1, cx);
    assert!(esc_held(&owner, cx), "the hold takes Esc");
    owner.update(cx, |owner, _| {
        owner.voice.pending.as_mut().expect("the hold").source = Source::Sent;
    });
    let origin = Some(RunOrigin::Hold {
        gesture: GestureId(1),
    });
    run_state(&owner, 1, Phase::Thinking, origin, cx);
    assert!(
        esc_held(&owner, cx),
        "the hold's question is still being taken"
    );
    run_state(&owner, 1, Phase::Answering, None, cx);
    assert!(
        !esc_held(&owner, cx),
        "the hold's own answer gives Esc back"
    );
}
