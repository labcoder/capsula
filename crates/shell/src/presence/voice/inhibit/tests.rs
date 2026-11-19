//! The D2b rows: a gesture's capture inhibit from key-down to its proved retirement, on the
//! real owner with a scripted speaker.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{
    ControlToBrain, GestureId, HoldFailure, HoldStatus, PartState, Phase, RunId, ThreadId,
};
use kumochi_shell_core::speaker::{Command, Snapshot, Tail};

use super::{Authority, Gates};
use crate::input::hold::Intent;
use crate::input::{Edge, Event, Key, PhysicalKey, TalkHeld};
use crate::platform::microphone::MicError;
use crate::platform::{Deliver, EventTime};
use crate::presence::voice::tests::{chord, native, source, until};
use crate::presence::voice_wiring_tests::{gate, hold, key_down, ready, status};
use crate::presence::{EscapePanel, Presence};
use crate::strings::Text;

impl Gates {
    /// Whether `gesture` has a gate, and whether it holds a token.
    pub(in crate::presence) fn held(&self, gesture: GestureId) -> Option<bool> {
        self.list
            .borrow()
            .iter()
            .find(|gate| gate.gesture == gesture)
            .map(|gate| matches!(gate.authority, Authority::Held(_)))
    }

    /// The run `gesture`'s gate fences.
    pub(in crate::presence) fn run(&self, gesture: GestureId) -> Option<(u64, RunId)> {
        self.list
            .borrow()
            .iter()
            .find(|gate| gate.gesture == gesture)
            .and_then(|gate| gate.run)
    }
}

const G1: GestureId = GestureId(1);
const G2: GestureId = GestureId(2);

/// Session 1's live output, with a tail the test keeps as a speaker's worker would.
fn speaker(owner: &Entity<Presence>, cx: &mut TestAppContext) -> (Receiver<Command>, Arc<Tail>) {
    owner.update(cx, |owner, _| owner.link.output().scripted(1))
}

/// Every pause, resume and stop the owner queues, with no live output: a hold can start.
fn witness(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Receiver<Command> {
    let (seen, commands) = std::sync::mpsc::channel();
    owner.update(cx, |owner, _| {
        owner.link.output().witness(Box::new(move |command| {
            let _seen = seen.send(*command);
        }));
    });
    commands
}

/// The pauses, resumes and stops the speaker was given, in order.
fn heard(commands: &Receiver<Command>) -> Vec<Command> {
    commands
        .try_iter()
        .filter(|command| {
            matches!(
                command,
                Command::Pause(_) | Command::Resume(_) | Command::Stop(_)
            )
        })
        .collect()
}

fn at(start: Instant, millis: u64) -> EventTime {
    EventTime {
        instant: start + Duration::from_millis(millis),
    }
}

fn deadline(deliver: &Deliver, start: Instant, millis: u64) {
    deliver(Event::Deadline, at(start, millis));
}

fn refuse(owner: &Entity<Presence>, calls: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, _| owner.link.output().refuse(calls));
}

fn sent(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| controls.try_recv().ok()).collect()
}

fn stopped(gesture: GestureId) -> ControlToBrain {
    ControlToBrain::VoiceHoldStopped { gesture }
}

fn cancel(gesture: GestureId) -> ControlToBrain {
    ControlToBrain::GestureCancel { gesture }
}

#[gpui::test]
fn voice_input_off_takes_no_inhibit(cx: &mut TestAppContext) {
    for on in [false, true] {
        let (owner, _data, _controls) = ready(cx);
        let (commands, _tail) = speaker(&owner, cx);
        owner.update(cx, |owner, cx| {
            if !on {
                owner.settings.update(cx, |settings, _| {
                    settings.receive_speech_input(Some(1), 1, PartState::NotInstalled);
                });
            }
            key_down(owner, 1, cx);
            assert_eq!(owner.voice.gates.held(G1), on.then_some(true));
        });
        cx.run_until_parked();
        let expected: &[Command] = if on { &[Command::Pause(1)] } else { &[] };
        assert_eq!(heard(&commands), expected, "voice on: {on}");
    }
}

#[gpui::test]
fn a_busy_key_down_still_reaches_the_microphone(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let commands = witness(&owner, cx);
    let starts = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&starts);
    owner.update(cx, |owner, _| {
        owner.voice.start = Some(Box::new(move |_, _| {
            counted.set(counted.get() + 1);
            Err(MicError::Unavailable)
        }));
    });
    refuse(&owner, 1, cx);
    hold(&owner, 1, cx);
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [Command::Pause(1)],
        "acquired on its retry"
    );
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    assert_eq!(starts.get(), 1, "the hold reached the microphone");
}

#[gpui::test]
fn a_busy_key_down_that_ends_or_goes_off_before_its_retry_takes_nothing(cx: &mut TestAppContext) {
    for cut in [None, Some("release"), Some("off")] {
        let (owner, _data, _controls) = ready(cx);
        let (commands, _tail) = speaker(&owner, cx);
        refuse(&owner, 1, cx);
        owner.update(cx, |owner, cx| {
            key_down(owner, 1, cx);
            assert_eq!(owner.voice.gates.held(G1), Some(false), "busy");
            match cut {
                Some("release") => owner.voice.gates.disengaged(),
                Some("off") => owner.voice.gates.follow(false),
                _ => {}
            }
        });
        cx.run_until_parked();
        let expected: &[Command] = if cut.is_none() {
            &[Command::Pause(1)]
        } else {
            &[]
        };
        assert_eq!(heard(&commands), expected, "{cut:?}");
        if cut.is_some() {
            owner.update(cx, |owner, cx| {
                owner.voice.gates.follow(true);
                key_down(owner, 2, cx);
            });
            cx.run_until_parked();
            assert_eq!(
                heard(&commands),
                [Command::Pause(1)],
                "the owner is free: {cut:?}"
            );
        }
    }
}

#[gpui::test]
fn a_first_tap_or_an_interrupted_chord_resumes_once(cx: &mut TestAppContext) {
    for interrupted in [false, true] {
        let (owner, _data, _controls) = ready(cx);
        let (commands, _tail) = speaker(&owner, cx);
        let deliver = native(&owner, cx);
        let start = Instant::now();
        cx.update(|_| {
            chord(&deliver, Edge::Down, start);
            if interrupted {
                let key = Key::Other(PhysicalKey::new(30).expect("a key"));
                deliver(
                    Event::Key {
                        key,
                        edge: Edge::Down,
                    },
                    at(start, 10),
                );
            } else {
                chord(&deliver, Edge::Up, start + Duration::from_millis(10));
                deadline(&deliver, start, 320);
            }
        });
        cx.run_until_parked();
        assert_eq!(
            heard(&commands),
            [Command::Pause(1), Command::Resume(1)],
            "interrupted: {interrupted}"
        );
    }
}

#[gpui::test]
fn a_second_press_moves_the_token_with_no_resume(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    let deliver = native(&owner, cx);
    let start = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, start);
        chord(&deliver, Edge::Up, start + Duration::from_millis(10));
        chord(&deliver, Edge::Down, start + Duration::from_millis(20));
        // The first tap's window, now stale.
        deadline(&deliver, start, 310);
    });
    cx.run_until_parked();
    assert_eq!(heard(&commands), [Command::Pause(1)]);
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.gates.held(G2), Some(true), "moved, not retired");
        assert_eq!(owner.voice.gates.held(G1), None);
    });
}

#[gpui::test]
fn a_busy_transfer_keeps_the_old_token_until_the_dispatcher_moves_it(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    let deliver = native(&owner, cx);
    let output = owner.read_with(cx, |owner, _| owner.link.clone());
    let start = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, start);
        chord(&deliver, Edge::Up, start + Duration::from_millis(10));
        output.output().refuse(1);
        chord(&deliver, Edge::Down, start + Duration::from_millis(20));
    });
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.voice.gates.held(G2),
            Some(false),
            "the move is pending"
        );
    });
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [Command::Pause(1)],
        "one pause, no resume"
    );
    owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.gates.held(G2), Some(true), "moved on the retry");
    });
}

#[gpui::test]
fn a_press_while_a_microphone_is_bound_is_refused_and_keeps_the_hold(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    hold(&owner, 1, cx);
    cx.run_until_parked();
    sent(&controls);
    owner.update(cx, |owner, cx| {
        gate(owner, 2, cx);
        let token = owner.link.microphone_gate().arm(G2);
        assert_eq!(
            token.unwrap_err(),
            MicError::Stale,
            "hold 1's generation is live"
        );
        owner.voice_intent(
            Intent::Recognized {
                gesture: G2,
                generation: Err(MicError::Stale),
            },
            owner.voice.pending.as_ref().expect("hold 1").stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        let pending = owner.voice.pending.as_ref().expect("hold 1 stays");
        assert_eq!(pending.token.gesture(), G1);
        assert_eq!(owner.voice.gates.held(G1), Some(true));
    });
    assert_eq!(heard(&commands), [Command::Pause(1)], "no resume");
    assert_eq!(sent(&controls), [], "hold 1 isn't cancelled");
}

#[gpui::test]
fn a_reset_with_no_hold_retires_so_the_next_press_acquires(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    let deliver = native(&owner, cx);
    let start = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, start);
        deliver(Event::Reset, at(start, 10));
    });
    cx.run_until_parked();
    cx.update(|_| {
        chord(&deliver, Edge::Up, start + Duration::from_millis(20));
        chord(&deliver, Edge::Down, start + Duration::from_millis(30));
    });
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [Command::Pause(1), Command::Resume(1), Command::Pause(2)]
    );
}

#[gpui::test]
fn an_unstarted_generation_is_discarded_once_and_the_brain_told(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    let (token, _) = hold(&owner, 1, cx);
    sent(&controls);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    assert!(token.retired());
    assert_eq!(heard(&commands), [Command::Pause(1), Command::Resume(1)]);
    assert_eq!(sent(&controls), [cancel(G1), stopped(G1)], "told once");
}

#[gpui::test]
fn a_failed_start_retires_through_the_gate(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    for worker in [true, false] {
        let (owner, _data, _controls) = ready(cx);
        let commands = witness(&owner, cx);
        let (token, _) = hold(&owner, 1, cx);
        owner.update(cx, |owner, _| {
            owner.voice.start = Some(Box::new(move |token, session| {
                if worker {
                    crate::platform::microphone::MicOwner::start_reserved_with_for_tests(
                        token,
                        session,
                        |_, _, _, _| {
                            Ok(crate::platform::microphone::Finalization::Discarded(
                                MicError::Unavailable,
                            ))
                        },
                    )
                } else {
                    Err(MicError::Unavailable)
                }
            }));
        });
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        until(cx, |cx| {
            owner.read_with(cx, |owner, _| owner.voice.pending.is_none())
        });
        until(cx, |_| token.retired());
        cx.run_until_parked();
        assert_eq!(
            heard(&commands),
            [Command::Pause(1), Command::Resume(1)],
            "worker: {worker}"
        );
    }
}

/// A hold whose microphone runs on the fake source until `finish`.
fn listening(
    owner: &Entity<Presence>,
    failure: Option<MicError>,
    cx: &mut TestAppContext,
) -> (
    std::sync::mpsc::Sender<()>,
    crate::platform::microphone::Generation,
) {
    let (open, finish, started, _samples) = source(owner, cx, failure);
    let (token, _) = hold(owner, 1, cx);
    status(owner, 1, 1, HoldStatus::Admitted, cx);
    started
        .recv_timeout(Duration::from_secs(3))
        .expect("started");
    open.send(()).expect("opens");
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldListening)
        })
    });
    (finish, token)
}

#[gpui::test]
fn only_the_physical_receipt_retires_a_live_microphone(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let commands = witness(&owner, cx);
    let (finish, token) = listening(&owner, None, cx);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    assert!(!token.finished(), "the worker still runs");
    assert_eq!(
        heard(&commands),
        [Command::Pause(1)],
        "paused until the proof"
    );
    finish.send(()).expect("finishes");
    until(cx, |_| token.retired());
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [Command::Resume(1)],
        "exactly one retirement"
    );
    owner.update(cx, |owner, cx| key_down(owner, 2, cx));
    cx.run_until_parked();
    assert_eq!(heard(&commands), [Command::Pause(2)], "a new hold acquires");
}

#[gpui::test]
fn an_unconfirmed_microphone_keeps_speech_paused_and_says_so(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let commands = witness(&owner, cx);
    let (finish, token) = listening(&owner, Some(MicError::Unconfirmed), cx);
    owner.update(cx, Presence::cancel_voice);
    finish.send(()).expect("finishes");
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            owner.voice.status == Some(Text::HoldUnavailable)
        })
    });
    // Every later wake retries the refused proof, and none resumes.
    owner.update(cx, |owner, cx| key_down(owner, 2, cx));
    cx.run_until_parked();
    assert!(!token.retired());
    assert_eq!(heard(&commands), [Command::Pause(1)], "speech stays paused");
}

#[gpui::test]
fn a_late_receipt_marks_only_its_own_gate(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let (commands, _tail) = speaker(&owner, cx);
    let (old, _) = hold(&owner, 1, cx);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, G1);
    });
    hold(&owner, 2, cx);
    owner.read_with(cx, |owner, _| owner.voice.gates.received(&old));
    cx.run_until_parked();
    sent(&controls);
    status(&owner, 1, 2, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [
            Command::Pause(1),
            Command::Resume(1),
            Command::Pause(2),
            Command::Resume(2)
        ]
    );
    assert_eq!(
        sent(&controls),
        [cancel(G2), stopped(G2)],
        "hold 2 retired unstarted"
    );
}

/// Run 5 speaks on session 1 with nothing played yet.
fn answering(owner: &Entity<Presence>, cx: &mut TestAppContext) -> (Receiver<Command>, Arc<Tail>) {
    owner.update(cx, |owner, _| {
        owner
            .runs
            .apply_state(RunId(5), ThreadId(1), Phase::Answering, None);
        let (commands, tail) = owner.link.output().scripted(1);
        tail.publish(&Snapshot {
            exposed: 480,
            played: 0,
            flight: 0,
            quiet: 0,
            flags: 0,
            spoken: 5,
        });
        (commands, tail)
    })
}

/// Hold 1's `End` and Esc, in one delivery.
fn end_and_escape(owner: &Entity<Presence>, refused: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        let stamp = owner.voice.pending.as_ref().expect("hold 1").stamp;
        owner.voice_intent(
            Intent::End {
                gesture: G1,
                cancelled: true,
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        owner.link.output().refuse(refused);
        owner.activity = Presence::activity(&owner.runs, EscapePanel::Closed, &owner.presenter);
        owner.escape(cx).expect("Esc");
    });
}

#[gpui::test]
fn end_and_esc_in_one_delivery_stop_before_any_resume(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, cx);
    hold(&owner, 1, cx);
    end_and_escape(&owner, 0, cx);
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [
            Command::Pause(1),
            Command::Stop(RunId(5)),
            Command::Resume(1)
        ]
    );
}

#[gpui::test]
fn a_pending_speaker_stop_holds_the_resume_back(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (commands, _tail) = answering(&owner, cx);
    hold(&owner, 1, cx);
    end_and_escape(&owner, 1, cx);
    owner.read_with(cx, |owner, _| {
        assert!(
            !owner.pending_stops.is_empty(),
            "the busy owner deferred the stop"
        );
    });
    for _ in 0..10 {
        cx.background_executor
            .advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
    }
    assert_eq!(
        heard(&commands),
        [
            Command::Pause(1),
            Command::Stop(RunId(5)),
            Command::Resume(1)
        ]
    );
}

#[gpui::test]
fn a_busy_retire_after_the_last_receipt_retries_by_itself(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let commands = witness(&owner, cx);
    let (finish, token) = listening(&owner, None, cx);
    status(&owner, 1, 1, HoldStatus::Failed(HoldFailure::Busy), cx);
    cx.run_until_parked();
    refuse(&owner, 1, cx);
    finish.send(()).expect("finishes");
    until(cx, |_| token.retired());
    cx.run_until_parked();
    assert_eq!(heard(&commands), [Command::Pause(1)], "the retire was busy");
    cx.background_executor
        .advance_clock(Duration::from_millis(10));
    cx.run_until_parked();
    assert_eq!(
        heard(&commands),
        [Command::Resume(1)],
        "retried with no other wake"
    );
}

/// Counts starts; each start fails, so a hold starts at most once.
fn starts(owner: &Entity<Presence>, cx: &mut TestAppContext) -> std::rc::Rc<std::cell::Cell<u32>> {
    let started = std::rc::Rc::<std::cell::Cell<u32>>::default();
    let count = std::rc::Rc::clone(&started);
    owner.update(cx, |owner, _| {
        owner.voice.start = Some(Box::new(move |_, _| {
            count.set(count.get() + 1);
            Err(MicError::Unavailable)
        }));
    });
    started
}

fn wake(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, _| owner.voice.gates.poke());
    cx.run_until_parked();
}

/// Session 1's tail: never exposed, quiet under serial `quiet`.
fn quiet(tail: &Tail, quiet: u32) {
    tail.publish(&Snapshot {
        exposed: 0,
        played: 0,
        flight: 0,
        quiet,
        flags: 0,
        spoken: 0,
    });
}

#[gpui::test]
fn no_start_until_a_valid_admission(cx: &mut TestAppContext) {
    for admitted in [false, true] {
        let (owner, _data, _controls) = ready(cx);
        let started = starts(&owner, cx);
        hold(&owner, 1, cx);
        wake(&owner, cx);
        assert_eq!(started.get(), 0, "quiet, but not admitted");
        let status_now = if admitted {
            HoldStatus::Admitted
        } else {
            HoldStatus::Failed(HoldFailure::Busy)
        };
        status(&owner, 1, 1, status_now, cx);
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        wake(&owner, cx);
        assert_eq!(started.get(), u32::from(admitted), "admitted: {admitted}");
    }
}

#[gpui::test]
fn admission_and_quiet_in_either_order_start_once(cx: &mut TestAppContext) {
    for quiet_first in [true, false] {
        let (owner, _data, _controls) = ready(cx);
        let (_commands, tail) = speaker(&owner, cx);
        quiet(&tail, 0);
        let started = starts(&owner, cx);
        hold(&owner, 1, cx);
        if quiet_first {
            quiet(&tail, 1);
        }
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        assert_eq!(
            started.get(),
            u32::from(quiet_first),
            "checked at admission"
        );
        quiet(&tail, 1);
        wake(&owner, cx);
        wake(&owner, cx);
        assert_eq!(started.get(), 1, "quiet first: {quiet_first}");
    }
}

#[gpui::test]
fn a_busy_owner_gets_three_rechecks_and_wakes_never_refill_them(cx: &mut TestAppContext) {
    for (refused, expected) in [(3, 1), (4, 0)] {
        let (owner, _data, _controls) = ready(cx);
        let started = starts(&owner, cx);
        hold(&owner, 1, cx);
        cx.run_until_parked();
        refuse(&owner, refused, cx);
        status(&owner, 1, 1, HoldStatus::Admitted, cx);
        for _ in 0..5 {
            wake(&owner, cx);
            cx.background_executor
                .advance_clock(Duration::from_millis(10));
            cx.run_until_parked();
        }
        assert_eq!(started.get(), expected, "{refused} busy reads");
        if expected == 0 {
            owner.read_with(cx, |owner, _| {
                assert!(owner.voice.pending.is_none(), "denied");
                assert_eq!(owner.voice.status, Some(Text::TurnBusy));
            });
        }
    }
}

#[gpui::test]
fn the_quiet_deadline_expires_when_reached_exactly(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (_commands, tail) = speaker(&owner, cx);
    quiet(&tail, 0);
    let started = starts(&owner, cx);
    hold(&owner, 1, cx);
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    cx.background_executor.advance_clock(super::QUIET_WAIT);
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none(), "expired");
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
    });
    quiet(&tail, 1);
    wake(&owner, cx);
    assert_eq!(started.get(), 0);
}

#[gpui::test]
fn a_recording_outlives_the_quiet_deadline(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let (_finish, _token) = listening(&owner, None, cx);
    cx.background_executor
        .advance_clock(super::QUIET_WAIT + Duration::from_secs(1));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_some(), "still recording");
        assert_eq!(owner.voice.status, Some(Text::HoldListening));
    });
}

#[gpui::test]
fn an_output_change_before_running_drains_the_worker_and_starts_no_other(cx: &mut TestAppContext) {
    let (owner, data, _controls) = ready(cx);
    let (open, finish, started, _samples) = source(&owner, cx, None);
    hold(&owner, 1, cx);
    status(&owner, 1, 1, HoldStatus::Admitted, cx);
    started
        .recv_timeout(Duration::from_secs(3))
        .expect("started");
    // The claimed no-output stretch ends: a speaker opens before the worker runs.
    let (_commands, _tail) = speaker(&owner, cx);
    open.send(()).expect("opens");
    finish.send(()).expect("drains");
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| owner.voice.pending.is_none())
    });
    owner.read_with(cx, |owner, _| {
        assert_ne!(owner.voice.status, Some(Text::HoldListening));
    });
    assert!(started.try_recv().is_err(), "no second start");
    assert!(data.is_empty(), "nothing collected");
}

#[gpui::test]
fn a_proof_taken_on_a_lost_session_is_owed_to_the_next_connection(cx: &mut TestAppContext) {
    use kumochi_protocol::message::Incarnation;
    let (owner, _data, old_controls) = ready(cx);
    hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.voice_status(1, G1, HoldStatus::Cancelled, cx);
        let old = owner.session.take().expect("connected");
        owner.end_session(&old, cx).detach();
    });
    cx.run_until_parked();
    let controls = owner.update(cx, |owner, _| {
        let controls = owner.link.record_controls();
        owner.session = Some(crate::link::Session::detached(2, Incarnation(2)));
        owner.runs.met(Incarnation(2));
        owner.runs.connected(2);
        owner.runs.settings(ThreadId(1));
        owner.link.replay_recorded_microphone_stop(2);
        controls
    });
    let told = |controls: &async_channel::Receiver<ControlToBrain>| {
        std::iter::from_fn(|| controls.try_recv().ok())
            .filter(|message| *message == stopped(G1))
            .count()
    };
    assert_eq!(told(&old_controls) + told(&controls), 1, "owed once");
    owner.update(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(2, G1);
        assert!(!owner.link.voice_stop_pending(), "acknowledged");
    });
    hold(&owner, 2, cx);
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner
                .voice
                .pending
                .as_ref()
                .map(|hold| hold.token.gesture()),
            Some(G2),
            "a new hold is admitted"
        );
        assert_ne!(owner.voice.status, Some(Text::TurnBusy));
    });
}

#[gpui::test]
fn a_discard_proved_after_a_newer_arm_still_tells_the_brain(cx: &mut TestAppContext) {
    let (owner, _data, controls) = ready(cx);
    let started = starts(&owner, cx);
    hold(&owner, 1, cx);
    cx.run_until_parked();
    let _newer = owner.update(cx, |owner, cx| {
        owner.voice_status(1, G1, HoldStatus::Admitted, cx);
        // The failed start retired gesture 1; the native callback arms the next press.
        owner
            .link
            .microphone_gate()
            .arm(G2)
            .expect("gesture 1 retired")
    });
    cx.run_until_parked();
    assert_eq!(started.get(), 1);
    let told = sent(&controls)
        .into_iter()
        .filter(|message| *message == stopped(G1))
        .count();
    assert_eq!(told, 1, "the brain is told once");
}

#[gpui::test]
fn an_esc_waiting_for_a_busy_owner_holds_the_resume_until_it_is_decided(cx: &mut TestAppContext) {
    for stops in [true, false] {
        let (owner, _data, _controls) = ready(cx);
        let (commands, _tail) = answering(&owner, cx);
        let deliver = native(&owner, cx);
        hold(&owner, 1, cx);
        owner.update(cx, |owner, cx| {
            let stamp = owner.voice.pending.as_ref().expect("hold 1").stamp;
            owner.voice_intent(
                Intent::End {
                    gesture: G1,
                    cancelled: true,
                },
                stamp,
                0,
                None,
                TalkHeld::default(),
                cx,
            );
            owner.activity = Presence::activity(&owner.runs, EscapePanel::Closed, &owner.presenter);
            if !stops {
                // The hold's session is gone, so its press decides to stop nothing.
                let session = owner.session.as_ref().expect("connected");
                session
                    .alive()
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            // The owner is busy here, so the press waits in the delivery queue.
            deliver(
                Event::Escape,
                EventTime {
                    instant: Instant::now(),
                },
            );
            // The dispatcher's wake comes before the queued press is retried.
            owner.dispatch(cx);
        });
        assert_eq!(heard(&commands), [Command::Pause(1)], "held for the Esc");
        cx.run_until_parked();
        let expected: &[Command] = if stops {
            &[Command::Stop(RunId(5)), Command::Resume(1)]
        } else {
            &[Command::Resume(1)]
        };
        assert_eq!(heard(&commands), expected, "stops: {stops}");
    }
}
