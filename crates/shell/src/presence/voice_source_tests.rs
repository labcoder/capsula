use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{MicPcm, RunId};

use super::super::voice_wiring_tests::{hold, ready, status};
use super::*;
use crate::link::{Batch, LinkEvent};
use crate::platform::microphone::{MicError, Running};

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
