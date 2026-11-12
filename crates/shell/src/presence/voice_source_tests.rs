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
                    MicOwner::start_with(token, session, move |_, _, _, _| {
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
        owner.voice_intent(Intent::Unavailable, stamp, 0, cx);
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
            key: Key::Other(42),
            edge: Edge::Down,
        },
        EventTime { instant: at },
    );
    chord(&deliver, Edge::Down, at);
    deliver(
        Event::Deadline,
        EventTime {
            instant: at + Duration::from_millis(301),
        },
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
        EventTime {
            instant: at + Duration::from_millis(301),
        },
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

fn native(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Deliver {
    let weak = owner.downgrade();
    owner.update(cx, |owner, cx| {
        super::super::gesture::deliver(
            Rc::clone(&owner.gesture),
            owner.dock_window,
            weak,
            owner.link.clone(),
            Rc::clone(&owner.voice.target),
            cx,
        )
    })
}

fn chord(deliver: &Deliver, edge: Edge, at: Instant) {
    for key in [Key::LeftControl, Key::LeftAlt] {
        deliver(Event::Key { key, edge }, EventTime { instant: at });
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
        EventTime {
            instant: at + Duration::from_millis(301),
        },
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
                    MicOwner::start_with(token, session, move |_, _, _, _| {
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
        assert!(token.retired());
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(1)
            }
        );
        assert_eq!(
            controls.try_recv().unwrap(),
            ControlToBrain::GestureCancel {
                gesture: GestureId(1)
            }
        );
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
    owner.update(cx, |owner, cx| {
        owner.runs.disconnected();
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
            cx,
        );
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
        assert!(owner.voice.pending.is_none());
        assert!(token.retired());
    });
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
                    settings.receive_speech_input(Some(1), 1, state);
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
                EventTime {
                    instant: at + Duration::from_millis(300),
                },
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
            EventTime {
                instant: at + Duration::from_millis(300),
            },
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
            EventTime {
                instant: at + Duration::from_millis(340),
            },
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
            EventTime {
                instant: at + Duration::from_millis(300),
            },
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
                key: Key::Other(1),
                edge: Edge::Down,
            },
            EventTime {
                instant: at + Duration::from_millis(310),
            },
        );
        deliver(
            Event::Escape,
            EventTime {
                instant: at + Duration::from_millis(311),
            },
        );
        deliver(
            Event::Key {
                key: Key::Other(1),
                edge: Edge::Up,
            },
            EventTime {
                instant: at + Duration::from_millis(312),
            },
        );
        chord(&deliver, Edge::Up, at + Duration::from_millis(320));
        chord(&deliver, Edge::Down, at + Duration::from_millis(330));
        deliver(
            Event::Deadline,
            EventTime {
                instant: at + Duration::from_millis(630),
            },
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

#[gpui::test]
fn stale_deferred_escape_marker_ends_on_raw_key_up_without_a_hotkey(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let deliver = native(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        deliver(
            Event::Deadline,
            EventTime {
                instant: at + Duration::from_millis(300),
            },
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
                key: Key::Other(1),
                edge: Edge::Down,
            },
            EventTime {
                instant: at + Duration::from_millis(310),
            },
        );
        deliver(
            Event::Key {
                key: Key::Other(1),
                edge: Edge::Up,
            },
            EventTime {
                instant: at + Duration::from_millis(312),
            },
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
            EventTime {
                instant: at + Duration::from_millis(640),
            },
        );
    });
    cx.run_until_parked();
    owner.update(cx, |owner, _| {
        assert!(!owner.runs.working());
    });
}

fn until(cx: &mut TestAppContext, ready: impl Fn(&mut TestAppContext) -> bool) {
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

fn source(
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
            MicOwner::start_with(generation, session, move |token, session, running, pcm| {
                entered.send(()).unwrap();
                opening.recv_timeout(Duration::from_secs(3)).unwrap();
                let prepared = token.running();
                let mut collected = false;
                if prepared.is_ok() {
                    running
                        .try_send(Running {
                            gesture: token.gesture(),
                            rate: 8_000,
                        })
                        .unwrap();
                    while token.mode(session, Instant::now()) == Ok(Mode::Preparing) {
                        thread::park_timeout(Duration::from_millis(1));
                    }
                    if token.mode(session, Instant::now()) == Ok(Mode::Collecting) {
                        collected = true;
                        copied.send(()).unwrap();
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
                            rate: 8_000,
                            last: true,
                            samples: vec![17, -93, 405],
                        })
                        .unwrap();
                        Ok(Finalization::Released { packets: 1 })
                    }
                    Err(error) => Ok(Finalization::Discarded(error)),
                    _ => panic!("fake source must be stopped before finalization"),
                }
            })
        }));
    });
    (open, finish, started, samples)
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
                rate: 8_000,
                last: true,
                samples: vec![17, -93, 405],
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

fn after(at: Instant, millis: u64) -> EventTime {
    EventTime {
        instant: at + Duration::from_millis(millis),
    }
}

fn listen(
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
                owner.runs.begin_hold(GestureId(1)).unwrap();
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
