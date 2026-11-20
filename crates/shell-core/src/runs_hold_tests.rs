use kumochi_protocol::message::{GestureId, ScreenChoice};

use super::*;
use crate::strings::Text;

fn ready() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs
}

#[test]
fn owned_stop_registration_is_released_without_allocating_on_duplicate_or_raw_end() {
    let mut runs = ready();
    state(&mut runs, 7, 1, 1, Phase::Thinking);
    let tail = RunTail {
        run: RunId(7),
        thread: ThreadId(1),
        control_closed: false,
        data_ended: false,
    };
    let (stop, counts, bytes) = crate::allocation::usage(|| runs.stop_owned(tail));
    assert_eq!(
        stop,
        Some(ControlToBrain::Stop {
            run: RunId(7),
            revealed: Vec::new()
        })
    );
    assert_eq!(counts, [1, 0]);
    assert!(bytes[0] > 0);
    assert_eq!(bytes[1], 0);
    let (stop, duplicate_counts, duplicate_bytes) =
        crate::allocation::usage(|| runs.stop_owned(tail));
    assert_eq!(stop, None);
    assert_eq!((duplicate_counts, duplicate_bytes), ([0, 0], [0, 0]));
    let (credit, raw_counts, raw_bytes) =
        crate::allocation::usage(|| runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4));
    assert_eq!(credit, 4);
    assert_eq!((raw_counts, raw_bytes), ([0, 0], [0, 0]));
    let ((applied, notice), released_counts, released_bytes) = crate::allocation::usage(|| {
        runs.apply_run_state(RunId(7), ThreadId(1), Phase::Failed, None, None)
    });
    assert!(!applied.visible);
    assert_eq!(applied.restore, None);
    assert_eq!(notice, None);
    assert_eq!(released_counts, [0, 1]);
    assert_eq!(released_bytes, [0, bytes[0]]);
    assert!(runs.pending_stops.is_empty());
    println!(
        "owned_stop_allocation register_count={} register_bytes={} release_count={} release_bytes={} duplicate_allocations=0 raw_end_allocations=0",
        counts[0], bytes[0], released_counts[1], released_bytes[1]
    );
}

#[test]
fn owned_stops_preserve_pending_turns_and_retire_only_after_both_input_authorities() {
    let mut runs = ready();
    state(&mut runs, 7, 1, 1, Phase::Thinking);
    runs.submit("pending typed words", |_| true)
        .expect("queued");
    let tail = RunTail {
        run: RunId(7),
        thread: ThreadId(1),
        control_closed: false,
        data_ended: false,
    };
    assert_eq!(runs.stop_owned(tail), stop(7));
    state(&mut runs, 8, 2, 2, Phase::Thinking);
    let second = RunTail {
        run: RunId(8),
        thread: ThreadId(2),
        ..tail
    };
    assert_eq!(runs.stop_owned(second), stop(8));
    assert_eq!(runs.pending_stops.len(), 2);
    assert_eq!(text(&mut runs, 7), 19);
    assert_eq!(text(&mut runs, 8), 19);
    assert!(runs.shown().is_none());
    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
    assert_eq!(runs.stop_owned(tail), None);
    assert!(runs.pending_stops[&RunId(7)].data_ended);
    let applied = state(&mut runs, 7, 1, 1, Phase::Done);
    assert!(!applied.visible);
    assert_eq!(runs.pending_stops.len(), 1);
    assert_eq!(runs.owned_disposition(RunId(8)), RunDisposition::Stopped);
    assert_eq!(runs.submitted.len(), 1);
    let typed = runs.apply_state(
        RunId(9),
        ThreadId(1),
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert!(typed.visible);
    assert_eq!(typed.stop, None);
    assert_eq!(runs.stoppable(), Some(RunId(9)));
    assert_eq!(text(&mut runs, 9), 19);
    assert_eq!(runs.shown().expect("unowned answer").text, "answer");
    runs.disconnected();
    assert!(runs.pending_stops.is_empty());
    runs.connected(2);
    assert_eq!(runs.owned_disposition(RunId(8)), RunDisposition::Live);
}

#[test]
fn accepted_deletion_closes_only_its_thread_in_either_pipe_order() {
    for outcome in [Deleted::Hidden, Deleted::Removed] {
        for raw_first in [false, true] {
            for last_run in [None, Some(RunId(7))] {
                let mut runs = ready();
                for (run, thread) in [(7, 1), (8, 2)] {
                    state(&mut runs, run, thread, 1, Phase::Thinking);
                    runs.stop_owned(RunTail {
                        run: RunId(run),
                        thread: ThreadId(thread),
                        control_closed: false,
                        data_ended: false,
                    });
                }
                assert_eq!(runs.deleted(ThreadId(1), outcome, last_run), 0);
                assert!(!runs.pending_stops[&RunId(7)].control_closed);
                assert!(runs.confirm_delete(ThreadId(1)));
                assert_eq!(runs.deleted(ThreadId(1), Deleted::NotHidden, last_run), 0);
                assert!(!runs.pending_stops[&RunId(7)].control_closed);
                assert!(runs.confirm_delete(ThreadId(1)));
                if raw_first {
                    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
                }
                assert_eq!(runs.deleted(ThreadId(1), outcome, last_run), 0);
                if !raw_first {
                    assert_eq!(runs.owned_disposition(RunId(7)), RunDisposition::Stopped);
                    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
                }
                assert_eq!(runs.owned_disposition(RunId(7)), RunDisposition::Live);
                assert_eq!(runs.pending_stops.len(), 1);
                assert!(!runs.pending_stops[&RunId(8)].control_closed);
                assert_eq!(runs.deleted(ThreadId(2), outcome, None), 0);
                assert!(!runs.pending_stops[&RunId(8)].control_closed);
            }
        }
    }
}

#[test]
fn prebind_end_is_reported_and_completed_receipts_allocate_no_pending_stop() {
    let mut runs = ready();
    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 0);
    let applied = state(&mut runs, 7, 1, 1, Phase::Done);
    assert_eq!(applied.credit, 4);
    assert!(applied.input_ended);
    assert_eq!(
        runs.stop_owned(RunTail {
            run: RunId(7),
            thread: ThreadId(1),
            control_closed: true,
            data_ended: true
        }),
        None
    );
    assert!(runs.pending_stops.is_empty());
    assert!(!state(&mut runs, 8, 1, 2, Phase::Thinking).input_ended);
}

#[test]
fn automatic_hold_stop_folds_prebind_end_and_retires_on_its_two_authorities() {
    for deleted in [None, Some(Deleted::Hidden), Some(Deleted::Removed)] {
        for (prebind, raw_first) in [(true, true), (false, true), (false, false)] {
            let mut runs = ready();
            recognized(&mut runs, 1);
            runs.cancel_hold();
            assert_eq!(runs.take_hold_cancel(), cancel(1));
            assert!(runs.select_thread(Some(ThreadId(2))));
            recognized(&mut runs, 2);
            if prebind {
                assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 0);
            }
            let applied = state(&mut runs, 7, 1, 1, Phase::Thinking);
            assert_eq!(applied.stop, stop(7));
            assert!(!applied.visible);
            assert_eq!(applied.credit, if prebind { 4 } else { 0 });
            assert_eq!(applied.input_ended, prebind);
            assert_eq!(runs.pending_stops.len(), 1);
            assert_eq!(runs.pending_stops[&RunId(7)].data_ended, prebind);
            assert!(!runs.pending_stops[&RunId(7)].control_closed);
            assert_eq!(runs.hold.expect("newer pending hold").gesture, GestureId(2));
            assert!(state(&mut runs, 8, 2, 2, Phase::Thinking).visible);
            assert_eq!(runs.hold.expect("newer named hold").run, Some(RunId(8)));
            if raw_first && !prebind {
                assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
            }
            if let Some(outcome) = deleted {
                assert_eq!(runs.deleted(ThreadId(1), outcome, None), 0);
                assert!(!runs.pending_stops[&RunId(7)].control_closed);
                assert!(runs.confirm_delete(ThreadId(1)));
                assert_eq!(runs.deleted(ThreadId(1), Deleted::NotHidden, None), 0);
                assert!(!runs.pending_stops[&RunId(7)].control_closed);
                assert!(runs.confirm_delete(ThreadId(1)));
                assert_eq!(runs.deleted(ThreadId(1), outcome, None), 0);
            } else {
                let terminal = runs.apply_state(RunId(7), ThreadId(1), Phase::Done, None);
                assert!(!terminal.visible);
                assert_eq!(terminal.stop, None);
            }
            if !raw_first {
                assert!(runs.pending_stops[&RunId(7)].control_closed);
                assert!(!runs.pending_stops[&RunId(7)].data_ended);
                assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
            }
            assert!(runs.pending_stops.is_empty());
            assert_eq!(
                runs.hold.expect("newer hold survives closure").run,
                Some(RunId(8))
            );
            assert_eq!(runs.stoppable(), Some(RunId(8)));
            assert_eq!(state(&mut runs, 7, 1, 1, Phase::Thinking).stop, None);
            assert!(runs.pending_stops.is_empty());
        }
    }
}

#[test]
fn historical_automatic_hold_stop_cannot_recreate_retired_input_ownership() {
    let mut runs = ready();
    recognized(&mut runs, 1);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert_eq!(state(&mut runs, 7, 1, 1, Phase::Thinking).stop, stop(7));
    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(7) }, 4), 4);
    runs.apply_state(RunId(7), ThreadId(1), Phase::Done, None);
    assert!(runs.pending_stops.is_empty());
    recognized(&mut runs, 2);
    assert!(state(&mut runs, 8, 1, 2, Phase::Thinking).visible);
    runs.apply_state(RunId(8), ThreadId(1), Phase::Answering, None);
    assert_eq!(runs.apply_data(DataToShell::RunEnd { run: RunId(8) }, 4), 4);
    runs.apply_state(RunId(8), ThreadId(1), Phase::Done, None);
    recognized(&mut runs, 3);
    runs.apply_state(RunId(9), ThreadId(1), Phase::Thinking, None);
    let old = state(&mut runs, 8, 1, 2, Phase::Thinking);
    assert_eq!(old.stop, stop(8));
    assert!(!old.input_ended);
    assert!(!old.visible);
    assert!(runs.pending_stops.is_empty());
    assert_eq!(runs.hold.expect("newer pending hold").gesture, GestureId(3));
    assert_eq!(state(&mut runs, 8, 1, 2, Phase::Thinking).stop, None);
    assert!(runs.pending_stops.is_empty());
}

fn recognized(runs: &mut Runs, gesture: u32) {
    let thread = runs.thread();
    let selection = runs.revision();
    assert_eq!(
        runs.begin_hold(GestureId(gesture), None),
        Ok(ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(gesture),
            thread,
            selection,
        })
    );
}

fn state(runs: &mut Runs, run: u64, thread: u64, gesture: u32, phase: Phase) -> Applied {
    runs.apply_state(
        RunId(run),
        ThreadId(thread),
        phase,
        Some(RunOrigin::Hold {
            gesture: GestureId(gesture),
        }),
    )
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the expected control has the reducer's optional type"
)]
fn cancel(gesture: u32) -> Option<ControlToBrain> {
    Some(ControlToBrain::GestureCancel {
        gesture: GestureId(gesture),
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the expected control has the reducer's optional type"
)]
fn stop(run: u64) -> Option<ControlToBrain> {
    Some(ControlToBrain::Stop {
        run: RunId(run),
        revealed: Vec::new(),
    })
}

fn text(runs: &mut Runs, run: u64) -> u32 {
    runs.apply_data(
        DataToShell::AssistantText {
            run: RunId(run),
            delta: "answer".into(),
        },
        19,
    )
}

#[test]
fn exact_origin_binds_new_once_and_next_hold_uses_real_thread() {
    let mut runs = ready();
    assert!(runs.select_thread(None));
    recognized(&mut runs, 7);
    let unrequested = state(&mut runs, 1, 8, 99, Phase::Thinking);
    assert!(!unrequested.visible);
    assert_eq!(unrequested.stop, stop(1));
    assert_eq!(runs.target(), Target::New);
    assert_eq!(text(&mut runs, 2), 0);
    let applied = state(&mut runs, 2, 9, 7, Phase::Thinking);
    assert_eq!(applied.credit, 19);
    assert!(applied.visible);
    assert_eq!(applied.stop, None);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    assert_eq!(
        runs.shown().map(|shown| shown.text.as_str()),
        Some("answer")
    );
    assert!(!runs.focus(RunId(2), ThreadId(8)));
    assert!(state(&mut runs, 2, 8, 7, Phase::Thinking).visible);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    state(&mut runs, 2, 9, 7, Phase::Answering);
    recognized(&mut runs, 8);
    assert_eq!(runs.turns_sent, 0);
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert!(runs.submitted.is_empty());
    assert_eq!(state(&mut runs, 3, 10, 7, Phase::Thinking).stop, stop(3));
    assert_eq!(
        runs.hold.as_ref().map(|hold| hold.gesture),
        Some(GestureId(8))
    );
}

#[test]
fn exact_target_mismatch_never_binds_or_shows() {
    let mut runs = ready();
    assert!(runs.select_thread(Some(ThreadId(9))));
    recognized(&mut runs, 1);
    assert_eq!(text(&mut runs, 1), 0);
    let applied = state(&mut runs, 1, 8, 1, Phase::Thinking);
    assert_eq!(applied.stop, stop(1));
    assert_eq!(applied.credit, 19);
    assert!(!applied.visible);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    assert_eq!(runs.shown(), None);
    assert_eq!(runs.take_hold_cancel(), cancel(1));
}

#[test]
fn cancelled_old_first_state_cannot_take_over_a_newer_pending_or_named_hold() {
    let mut runs = ready();
    assert!(runs.select_thread(None));
    recognized(&mut runs, 1);
    assert!(runs.working());
    assert_eq!(runs.stop(), None);
    assert!(!runs.working());
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    recognized(&mut runs, 2);
    assert_eq!(text(&mut runs, 1), 0);
    let old = state(&mut runs, 1, 8, 1, Phase::Thinking);
    assert_eq!(old.stop, stop(1));
    assert_eq!(old.credit, 19);
    assert!(!old.visible);
    assert_eq!(runs.target(), Target::New);
    assert_eq!(runs.shown(), None);
    assert_eq!(
        runs.hold.as_ref().map(|hold| hold.gesture),
        Some(GestureId(2))
    );
    assert!(state(&mut runs, 2, 9, 2, Phase::Thinking).visible);
    assert_eq!(state(&mut runs, 1, 8, 1, Phase::Done).stop, None);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    assert_eq!(runs.live.map(|live| live.run), Some(RunId(2)));
    assert_eq!(text(&mut runs, 1), 19);
    assert_eq!(runs.shown(), None);
}

#[test]
fn cancellation_after_newer_named_run_still_fences_older_first_state() {
    let mut runs = ready();
    recognized(&mut runs, 1);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    recognized(&mut runs, 2);
    state(&mut runs, 2, 1, 2, Phase::Thinking);
    assert_eq!(state(&mut runs, 1, 1, 1, Phase::Thinking).stop, stop(1));
    assert_eq!(text(&mut runs, 1), 19);
    assert_eq!(runs.shown(), None);
    assert_eq!(runs.live.map(|live| live.run), Some(RunId(2)));
}

#[test]
fn selection_away_and_back_cancels_pending_and_thinking_holds() {
    for named in [false, true] {
        let mut runs = ready();
        assert!(runs.select_thread(Some(ThreadId(9))));
        recognized(&mut runs, 1);
        if named {
            state(&mut runs, 1, 9, 1, Phase::Thinking);
        }
        assert!(runs.select_thread(Some(ThreadId(8))));
        assert!(runs.select_thread(Some(ThreadId(9))));
        assert_eq!(runs.take_hold_cancel(), cancel(1));
        assert_eq!(runs.take_hold_cancel(), None);
        assert!(!state(&mut runs, 1, 9, 1, Phase::Answering).visible);
        assert_eq!(text(&mut runs, 1), 19);
        assert_eq!(runs.shown(), None);
        assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    }
}

#[test]
fn delete_cancels_without_a_typed_delete_watermark_or_resurrection() {
    let mut runs = ready();
    assert!(runs.select_thread(Some(ThreadId(9))));
    recognized(&mut runs, 1);
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert_eq!(runs.deleted(ThreadId(9), Deleted::Removed, None), 0);
    assert_eq!(runs.turns_sent, 0);
    assert_eq!(runs.gone, []);
    assert!(!runs.holds(ThreadId(9)));
    assert_eq!(state(&mut runs, 1, 9, 1, Phase::Thinking).stop, stop(1));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(text(&mut runs, 1), 19);
    assert_eq!(runs.shown(), None);
}

#[test]
fn deleting_newly_bound_hold_thread_cancels_its_named_correlation() {
    let mut runs = ready();
    assert!(runs.select_thread(None));
    recognized(&mut runs, 1);
    state(&mut runs, 1, 9, 1, Phase::Thinking);
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert!(!state(&mut runs, 1, 9, 1, Phase::Answering).visible);
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.gone, []);
}

#[test]
fn hold_failure_cannot_restore_a_typed_submission_or_offer_retry() {
    let mut runs = ready();
    assert_eq!(runs.submit("previous", |_| true), Ok(()));
    recognized(&mut runs, 1);
    let (applied, notice) = runs.apply_run_state(
        RunId(1),
        ThreadId(1),
        Phase::Failed,
        Some(ErrorKey::SessionEnded.into()),
        Some(RunOrigin::Hold {
            gesture: GestureId(1),
        }),
    );
    assert_eq!(applied.restore, None);
    assert_eq!(
        notice.map(|error| error.key()),
        Some(ErrorKey::SessionEnded)
    );
    assert_eq!(runs.submitted.len(), 1);
    assert_eq!(runs.submitted[0].turn, 1);
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert!(!runs.retry_ready(RunId(1)));
    assert_eq!(runs.take_hold_cancel(), None);
}

#[test]
fn typed_supersession_requires_cancellation_to_be_drained_before_send() {
    let mut runs = ready();
    recognized(&mut runs, 1);
    assert_eq!(
        runs.submit("newer", |_| panic!("cancel must go first")),
        Err(Refused::Busy)
    );
    assert_eq!(runs.turns_sent, 0);
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    let mut sent = None;
    assert_eq!(
        runs.submit("newer", |message| {
            sent = Some(message);
            true
        }),
        Ok(())
    );
    assert_eq!(
        sent,
        Some(DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "newer".into(),
        })
    );
    runs.apply_state(
        RunId(2),
        ThreadId(1),
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert!(!state(&mut runs, 1, 1, 1, Phase::Thinking).visible);
    assert_eq!(
        runs.live.map(|live| live.typed),
        Some(Some((1, runs.revision())))
    );
    assert_eq!(runs.kept(), [5]);
}

#[test]
fn busy_new_typed_target_and_gesture_high_water_are_preserved() {
    let mut runs = ready();
    assert!(runs.select_thread(None));
    assert_eq!(runs.submit("pending", |_| true), Ok(()));
    assert_eq!(runs.begin_hold(GestureId(1), None), Err(Refused::Busy));
    runs.refused(1, None);
    assert_eq!(runs.begin_hold(GestureId(1), None), Err(Refused::Busy));
    recognized(&mut runs, 2);
    assert_eq!(runs.begin_hold(GestureId(3), None), Err(Refused::Busy));
    runs.cancel_hold();
    assert_eq!(runs.begin_hold(GestureId(3), None), Err(Refused::Busy));
    assert_eq!(runs.take_hold_cancel(), cancel(2));
    assert_eq!(runs.begin_hold(GestureId(1), None), Err(Refused::Busy));
    recognized(&mut runs, u32::MAX);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(u32::MAX));
    assert_eq!(runs.begin_hold(GestureId(0), None), Err(Refused::Busy));
    assert_eq!(
        runs.begin_hold(GestureId(u32::MAX), None),
        Err(Refused::Busy)
    );
}

#[test]
fn answering_uses_ordinary_stop_and_explicit_model_cancel_fences_thinking() {
    let mut runs = ready();
    recognized(&mut runs, 1);
    state(&mut runs, 1, 1, 1, Phase::Thinking);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert!(!state(&mut runs, 1, 1, 1, Phase::Answering).visible);
    recognized(&mut runs, 2);
    state(&mut runs, 2, 1, 2, Phase::Answering);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), None);
    assert_eq!(runs.stop(), stop(2));
}

#[test]
fn disconnected_holds_cannot_revive_and_outer_debug_has_no_content() {
    let mut runs = ready();
    assert_eq!(runs.submit("PRIVATE_HOLD_SENTINEL", |_| true), Ok(()));
    recognized(&mut runs, 1);
    assert_eq!(
        runs.apply_data(
            DataToShell::AssistantText {
                run: RunId(1),
                delta: "PRIVATE_HOLD_SENTINEL".into(),
            },
            19
        ),
        0
    );
    assert_eq!(runs.held.len(), 1);
    let debug = format!("{runs:?}");
    assert!(!debug.contains("PRIVATE_HOLD_SENTINEL"));
    assert!(!debug.contains(&format!("{:?}", b"PRIVATE_HOLD_SENTINEL")));
    runs.disconnected();
    assert_eq!(runs.begin_hold(GestureId(2), None), Err(Refused::Offline));
    runs.connected(2);
    runs.settings(ThreadId(1));
    assert_eq!(runs.begin_hold(GestureId(1), None), Err(Refused::Busy));
    assert_eq!(state(&mut runs, 1, 1, 1, Phase::Thinking).stop, stop(1));
    assert_eq!(runs.begin_hold(GestureId(2), None), Err(Refused::Busy));
    recognized(&mut runs, 3);
    assert_eq!(runs.turns_sent, 0);
    assert_eq!(runs.take_hold_cancel(), None);
}

const SCREEN: ScreenHold = ScreenHold {
    pointer: kumochi_protocol::geometry::PhysicalPoint([-1_234_567, 7_654_321]),
    lease: kumochi_protocol::message::LeaseGeneration(424_242),
    t_qpc: 515_151_515,
};

#[test]
fn a_hold_is_a_screen_hold_only_when_it_is_given_a_screen() {
    use kumochi_protocol::message::{HeldScreen, ScreenClass};
    let mut runs = ready();
    recognized(&mut runs, 1);
    assert_eq!(runs.hold_screen_line(), None);
    let outcome = HeldScreen::NotIncluded(ScreenClass::Changed);
    assert!(!runs.hold_screen(GestureId(1), outcome));
    assert_eq!(runs.hold_screen_line(), None);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert_eq!(
        runs.begin_hold(GestureId(2), Some(SCREEN)),
        Ok(ControlToBrain::HoldRecognized {
            gesture: GestureId(2),
            thread: Some(ThreadId(1)),
            selection: 1,
            pointer: kumochi_protocol::geometry::PhysicalPoint([-1_234_567, 7_654_321]),
            lease: kumochi_protocol::message::LeaseGeneration(424_242),
            t_qpc: 515_151_515,
            notice: None,
        })
    );
    assert_eq!(runs.hold_screen_line(), Some(ScreenLine::WillSee));
    let debug = format!("{runs:?}");
    for part in ["1234567", "7654321", "424242", "515151515"] {
        assert!(!debug.contains(part), "{part}");
    }
}

#[test]
fn a_hold_refused_its_lease_says_the_screen_wasnt_ready_until_it_ends() {
    use kumochi_protocol::message::{HeldScreen, ScreenClass};
    let mut runs = ready();
    runs.hold_screen_refused(GestureId(1));
    assert_eq!(runs.hold_screen_line(), None);
    recognized(&mut runs, 1);
    runs.hold_screen_refused(GestureId(2));
    assert_eq!(runs.hold_screen_line(), None);
    runs.hold_screen_refused(GestureId(1));
    let line = Some(ScreenLine::NotIncluded(ScreenClass::NotReady));
    assert_eq!(runs.hold_screen_line(), line);
    // The brain was sent a voice-only hold, so no screen outcome is this hold's to show.
    assert!(!runs.hold_screen(GestureId(1), HeldScreen::Attached));
    assert_eq!(runs.hold_screen_line(), line);
    runs.cancel_hold();
    assert_eq!(runs.hold_screen_line(), None);
}

#[test]
fn a_screen_outcome_replaces_the_prediction_only_for_the_current_hold() {
    use kumochi_protocol::message::{HeldScreen, ScreenClass};
    let changed = HeldScreen::NotIncluded(ScreenClass::Changed);
    let mut runs = ready();
    runs.begin_hold(GestureId(1), Some(SCREEN)).expect("held");
    assert!(!runs.hold_screen(GestureId(2), changed));
    assert_eq!(runs.hold_screen_line(), Some(ScreenLine::WillSee));
    state(&mut runs, 1, 1, 1, Phase::Thinking);
    assert_eq!(runs.hold_screen_line(), Some(ScreenLine::WillSee));
    assert!(runs.hold_screen(
        GestureId(1),
        HeldScreen::NotIncluded(ScreenClass::NothingThere)
    ));
    let line = Some(ScreenLine::NotIncluded(ScreenClass::NothingThere));
    assert_eq!(runs.hold_screen_line(), line);
    assert!(!runs.hold_screen(GestureId(1), changed));
    assert_eq!(runs.hold_screen_line(), line);
    state(&mut runs, 1, 1, 1, Phase::Answering);
    assert_eq!(runs.hold_screen_line(), None);
    assert!(!runs.hold_screen(GestureId(1), changed));
    assert_eq!(runs.hold_screen_line(), None);

    for (gesture, outcome) in [(2, HeldScreen::Attached), (3, HeldScreen::Withdrawn)] {
        runs.begin_hold(GestureId(gesture), Some(SCREEN))
            .expect("held");
        assert!(runs.hold_screen(GestureId(gesture), outcome));
        assert_eq!(runs.hold_screen_line(), None);
        assert!(!runs.hold_screen(GestureId(gesture), changed));
        runs.cancel_hold();
        assert_eq!(runs.take_hold_cancel(), cancel(gesture));
    }
    runs.begin_hold(GestureId(4), Some(SCREEN)).expect("held");
    runs.cancel_hold();
    assert_eq!(runs.hold_screen_line(), None);
}

/// A send that wrote nothing tells no screen outcome: the window's prediction stays while the
/// request connects, and goes at the run's terminal state, whichever ending comes first.
#[test]
fn a_screen_hold_told_nothing_keeps_its_prediction_until_its_run_ends() {
    for state_first in [true, false] {
        let mut runs = ready();
        runs.begin_hold(GestureId(1), Some(SCREEN)).expect("held");
        state(&mut runs, 1, 1, 1, Phase::Thinking);
        assert_eq!(runs.hold_screen_line(), Some(ScreenLine::WillSee));
        let end = DataToShell::RunEnd { run: RunId(1) };
        if state_first {
            state(&mut runs, 1, 1, 1, Phase::Failed);
            assert_eq!(runs.hold_screen_line(), None);
            assert_eq!(runs.take_ended(), None, "one ending is in");
            runs.apply_data(end, 0);
        } else {
            runs.apply_data(end, 0);
            assert_eq!(runs.take_ended(), None, "one ending is in");
            state(&mut runs, 1, 1, 1, Phase::Failed);
        }
        assert_eq!(runs.hold_screen_line(), None, "state first: {state_first}");
        assert_eq!(
            runs.take_ended(),
            Some(RunId(1)),
            "state first: {state_first}"
        );
        assert_eq!(runs.take_ended(), None);
    }
}

#[test]
fn the_screen_holds_state_lasts_one_connection() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let on = ScreenHolds::On {
        model: ModelName::new("GPT-5.5").expect("valid"),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: kumochi_protocol::message::ModelSlug::new("fixture").expect("valid"),
        revision: kumochi_protocol::message::NoticeRevision::FIRST.next(),
    };
    let mut runs = ready();
    assert_eq!(runs.screen_holds(), None);
    runs.set_screen_holds(on.clone());
    assert_eq!(runs.screen_holds(), Some(&on));
    runs.set_screen_holds(ScreenHolds::Off(ScreenOff::TurnedOff));
    assert_eq!(
        runs.screen_holds(),
        Some(&ScreenHolds::Off(ScreenOff::TurnedOff))
    );
    runs.set_screen_holds(on.clone());
    runs.disconnected();
    assert_eq!(runs.screen_holds(), None);
    runs.set_screen_holds(on);
    runs.connected(2);
    assert_eq!(runs.screen_holds(), None);
}

#[test]
fn the_standing_notice_gives_way_in_order_and_names_only_the_newest_default() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let luna = ModelName::new("GPT-6 Luna").expect("valid");
    let sol = ModelName::new("GPT-6 Sol").expect("valid");
    let on = |model: &ModelName| {
        Some(ScreenHolds::On {
            model: model.clone(),
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
            slug: kumochi_protocol::message::ModelSlug::new("fixture").expect("valid"),
            revision: kumochi_protocol::message::NoticeRevision::FIRST.next(),
        })
    };
    let off = |why| Some(ScreenHolds::Off(why));
    // (stored state, the target thread has its own model, the default's name) and the notice.
    for (stored, own, default, notice) in [
        (None, false, Some(&luna), ScreenNotice::NotReady),
        (None, true, Some(&luna), ScreenNotice::OwnModel),
        (
            off(ScreenOff::Unsupported),
            true,
            Some(&luna),
            ScreenNotice::Hidden,
        ),
        (
            off(ScreenOff::TurnedOff),
            true,
            Some(&luna),
            ScreenNotice::TurnedOff,
        ),
        (
            off(ScreenOff::ModelCantSee),
            true,
            Some(&luna),
            ScreenNotice::OwnModel,
        ),
        (
            off(ScreenOff::NotReady),
            true,
            Some(&luna),
            ScreenNotice::OwnModel,
        ),
        (on(&luna), true, Some(&luna), ScreenNotice::OwnModel),
        (on(&luna), false, Some(&luna), ScreenNotice::On(&luna)),
        (on(&sol), false, Some(&luna), ScreenNotice::NotReady),
        (on(&luna), false, None, ScreenNotice::NotReady),
        (
            off(ScreenOff::ModelCantSee),
            false,
            Some(&luna),
            ScreenNotice::ModelCantSee(&luna),
        ),
        (
            off(ScreenOff::ModelCantSee),
            false,
            None,
            ScreenNotice::NotReady,
        ),
        (
            off(ScreenOff::NotReady),
            false,
            Some(&luna),
            ScreenNotice::NotReady,
        ),
    ] {
        let mut runs = ready();
        if let Some(stored) = stored.clone() {
            runs.set_screen_holds(stored);
        }
        assert_eq!(runs.screen_notice(own, default), notice, "{stored:?} {own}");
    }
}

#[test]
fn a_holds_prediction_names_the_notices_model_and_goes_quiet_when_the_notice_changes() {
    use kumochi_protocol::message::{HeldScreen, ModelName, ScreenClass};
    let luna = ModelName::new("GPT-6 Luna").expect("valid");
    let mut runs = ready();
    assert_eq!(runs.hold_screen_text(ScreenNotice::On(&luna)), None);
    runs.begin_hold(GestureId(1), Some(SCREEN)).expect("held");
    assert_eq!(
        runs.hold_screen_text(ScreenNotice::On(&luna)),
        Some((Text::ScreenHoldWillSee, Some(&luna)))
    );
    for stale in [
        ScreenNotice::NotReady,
        ScreenNotice::OwnModel,
        ScreenNotice::TurnedOff,
        ScreenNotice::ModelCantSee(&luna),
    ] {
        assert_eq!(runs.hold_screen_text(stale), None);
    }
    assert!(runs.hold_screen(
        GestureId(1),
        HeldScreen::NotIncluded(ScreenClass::NothingThere)
    ));
    // The reason names no model, so it stands whatever the notice says now.
    for notice in [ScreenNotice::On(&luna), ScreenNotice::NotReady] {
        assert_eq!(
            runs.hold_screen_text(notice),
            Some((Text::ScreenNotIncludedNothingThere, None))
        );
    }
}

/// A hold carries the revision of the `On` showing when the user began it to its release, never a
/// later one, and none when no `On` was showing.
#[test]
fn a_hold_carries_the_notice_it_began_under_to_its_release() {
    use kumochi_protocol::message::{
        ConnectionKind, ModelSlug, NoticeRevision, ScreenHolds, ScreenOff,
    };
    let mut runs = ready();
    let luna = shown_to_luna(&mut runs);
    let on = Some(&luna);
    let shown = Some(NoticeRevision::FIRST.next());
    let notice = |message: Option<ControlToBrain>| match message {
        Some(
            ControlToBrain::HoldRecognized { notice, .. }
            | ControlToBrain::ScreenHoldRecognized { notice, .. }
            | ControlToBrain::ScreenRelease { notice, .. },
        ) => notice,
        other => panic!("not a screen hold's control: {other:?}"),
    };
    assert_eq!(
        notice(runs.begin_hold(GestureId(1), Some(SCREEN)).ok()),
        shown
    );
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    let begun = runs.begin_screen_hold(GestureId(2), 1, false, on).ok();
    assert_eq!(notice(begun), shown);
    runs.set_screen_holds(ScreenHolds::On {
        model: luna.clone(),
        connection: ConnectionKind::ChatGpt,
        slug: ModelSlug::new("fixture").expect("valid"),
        revision: NoticeRevision::FIRST.next().next(),
    });
    assert_eq!(
        notice(runs.screen_release(GestureId(2), false, on, release())),
        shown
    );
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(2));
    runs.set_screen_holds(ScreenHolds::Off(ScreenOff::ModelCantSee));
    assert_eq!(
        notice(runs.begin_hold(GestureId(3), Some(SCREEN)).ok()),
        None
    );
}

fn release() -> HoldRelease {
    use kumochi_protocol::geometry::{PhysicalPoint, PointerPath};
    HoldRelease {
        pointer: PhysicalPoint([-2_345_678, 8_765_432]),
        path: PointerPath(vec![PhysicalPoint([3_456_789, -9_876_543])]),
        lease: kumochi_protocol::message::LeaseGeneration(434_343),
        t_qpc: 616_161_616,
        excluded_qpc: 717_171_717,
    }
}

/// Stores the brain's word that a hold shows the screen to the model it returns; with that
/// model as the default and a thread on the default, the standing notice is `On`.
fn shown_to_luna(runs: &mut Runs) -> kumochi_protocol::message::ModelName {
    use kumochi_protocol::message::{ModelName, ScreenHolds};
    let model = ModelName::new("GPT-6 Luna").expect("valid");
    runs.set_screen_holds(ScreenHolds::On {
        model: model.clone(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: kumochi_protocol::message::ModelSlug::new("fixture").expect("valid"),
        revision: kumochi_protocol::message::NoticeRevision::FIRST.next(),
    });
    model
}

#[test]
fn a_hold_that_shows_the_screen_at_release_is_admitted_as_any_hold_is() {
    let mut runs = ready();
    let luna = shown_to_luna(&mut runs);
    let begin = |runs: &mut Runs, gesture, t_qpc| {
        runs.begin_screen_hold(GestureId(gesture), t_qpc, false, Some(&luna))
    };
    assert_eq!(
        begin(&mut runs, 1, 515_151_515),
        Ok(ControlToBrain::ScreenHoldRecognized {
            gesture: GestureId(1),
            thread: Some(ThreadId(1)),
            selection: 1,
            t_qpc: 515_151_515,
            notice: Some(kumochi_protocol::message::NoticeRevision::FIRST.next()),
        })
    );
    // It predicts nothing, so it has no line unless something refuses its screen.
    assert_eq!(runs.hold_screen_line(), None);
    assert_eq!(
        runs.hold_screen_text(runs.screen_notice(false, Some(&luna))),
        None
    );
    assert!(!format!("{runs:?}").contains("515151515"));
    // One hold at a time, a cancel delivered before the next, and each gesture used once.
    assert_eq!(begin(&mut runs, 2, 1), Err(Refused::Busy));
    runs.cancel_hold();
    assert_eq!(begin(&mut runs, 3, 1), Err(Refused::Busy));
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert_eq!(begin(&mut runs, 3, 1), Err(Refused::Busy));
    runs.disconnected();
    assert_eq!(begin(&mut runs, 4, 1), Err(Refused::Offline));
    runs.connected(2);
    runs.settings(ThreadId(1));
    shown_to_luna(&mut runs);
    assert_eq!(begin(&mut runs, 4, 1), Err(Refused::Busy));
    // A typed turn waiting on a new thread refuses it, as it refuses a voice hold.
    assert!(runs.select_thread(None));
    assert_eq!(runs.submit("pending", |_| true), Ok(()));
    assert_eq!(begin(&mut runs, 5, 1), Err(Refused::Busy));
    runs.refused(1, None);
    // Its first Hold-origin state binds the new thread, as a voice hold's does.
    let selection = runs.revision();
    assert_eq!(
        begin(&mut runs, 6, 2),
        Ok(ControlToBrain::ScreenHoldRecognized {
            gesture: GestureId(6),
            thread: None,
            selection,
            t_qpc: 2,
            notice: Some(kumochi_protocol::message::NoticeRevision::FIRST.next()),
        })
    );
    assert!(state(&mut runs, 1, 9, 6, Phase::Thinking).visible);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
}

#[test]
fn a_hold_begun_while_the_notice_is_not_on_is_voice_only_and_never_releases_the_screen() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let luna = ModelName::new("GPT-6 Luna").expect("valid");
    let sol = ModelName::new("GPT-6 Sol").expect("valid");
    let on = ScreenHolds::On {
        model: luna.clone(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: kumochi_protocol::message::ModelSlug::new("fixture").expect("valid"),
        revision: kumochi_protocol::message::NoticeRevision::FIRST.next(),
    };
    let off = |why| Some(ScreenHolds::Off(why));
    // (what the brain said, the target thread has its own model, the default's name)
    for (stored, own, default) in [
        // Nothing said yet on this connection, as after a disconnect.
        (None, false, Some(&luna)),
        (Some(on.clone()), true, Some(&luna)),
        // The default just changed, so the stored `On` names the model before it.
        (Some(on.clone()), false, Some(&sol)),
        (Some(on.clone()), false, None),
        (off(ScreenOff::Unsupported), false, Some(&luna)),
        (off(ScreenOff::TurnedOff), false, Some(&luna)),
        (off(ScreenOff::ModelCantSee), false, Some(&luna)),
        (off(ScreenOff::NotReady), false, Some(&luna)),
    ] {
        let mut runs = ready();
        if let Some(stored) = stored.clone() {
            runs.set_screen_holds(stored);
        }
        assert_eq!(
            runs.begin_screen_hold(GestureId(1), 515_151_515, own, default),
            Ok(ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                thread: Some(ThreadId(1)),
                selection: 1,
            }),
            "{stored:?} {own}"
        );
        // The notice coming on afterwards changes nothing: this hold is voice only.
        runs.set_screen_holds(on.clone());
        assert!(
            !runs.releases_screen(GestureId(1), false, Some(&luna)),
            "{stored:?} {own}"
        );
        assert_eq!(
            runs.screen_release(GestureId(1), false, Some(&luna), release()),
            None,
            "{stored:?} {own}"
        );
    }
}

#[test]
fn only_the_current_hold_of_that_kind_builds_one_screen_release_while_the_notice_is_on() {
    use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};
    use kumochi_protocol::message::LeaseGeneration;
    let mut runs = ready();
    let luna = shown_to_luna(&mut runs);
    let on = Some(&luna);
    // No hold, a voice hold and a window hold build none.
    assert!(!runs.releases_screen(GestureId(1), false, on));
    assert_eq!(
        runs.screen_release(GestureId(1), false, on, release()),
        None
    );
    recognized(&mut runs, 1);
    assert!(!runs.releases_screen(GestureId(1), false, on));
    assert_eq!(
        runs.screen_release(GestureId(1), false, on, release()),
        None
    );
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    runs.begin_hold(GestureId(2), Some(SCREEN)).expect("held");
    assert!(!runs.releases_screen(GestureId(2), false, on));
    assert_eq!(
        runs.screen_release(GestureId(2), false, on, release()),
        None
    );
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(2));

    runs.begin_screen_hold(GestureId(3), 1, false, on)
        .expect("held");
    // Another gesture's release builds none, and refuses nothing of this hold.
    for other in [2, 4] {
        assert!(
            !runs.releases_screen(GestureId(other), false, on),
            "{other}"
        );
        assert_eq!(
            runs.screen_release(GestureId(other), false, on, release()),
            None,
            "{other}"
        );
    }
    assert!(runs.releases_screen(GestureId(3), false, on));
    let message = runs.screen_release(GestureId(3), false, on, release());
    assert_eq!(
        message,
        Some(ControlToBrain::ScreenRelease {
            gesture: GestureId(3),
            lease: LeaseGeneration(434_343),
            t_qpc: 616_161_616,
            excluded_qpc: 717_171_717,
            pointer: ReleasePointer(PhysicalPoint([-2_345_678, 8_765_432])),
            path: PointerPath(vec![PhysicalPoint([3_456_789, -9_876_543])]),
            notice: Some(kumochi_protocol::message::NoticeRevision::FIRST.next()),
        })
    );
    let (model, sent) = (format!("{runs:?}"), format!("{message:?}"));
    for part in ["2345678", "8765432", "3456789", "9876543"] {
        assert!(!model.contains(part) && !sent.contains(part), "{part}");
    }
    for part in ["434343", "616161616", "717171717"] {
        assert!(!model.contains(part), "{part}");
    }
    // One for each hold, and the hold itself goes on.
    assert!(!runs.releases_screen(GestureId(3), false, on));
    assert_eq!(
        runs.screen_release(GestureId(3), false, on, release()),
        None
    );
    assert!(state(&mut runs, 1, 1, 3, Phase::Thinking).visible);
}

#[test]
fn a_refusal_lands_once_on_a_hold_that_shows_the_screen_at_release_in_that_flow_s_words() {
    use kumochi_protocol::message::{HeldScreen, ScreenClass, ScreenHolds, ScreenOff};
    fn line<'a>(
        runs: &'a Runs,
        luna: &'a kumochi_protocol::message::ModelName,
    ) -> Option<(Text, Option<&'a kumochi_protocol::message::ModelName>)> {
        runs.hold_screen_text(runs.screen_notice(false, Some(luna)))
    }
    for (class, text) in [
        (ScreenClass::Changed, Text::ShowScreenChanged),
        (
            ScreenClass::TooLarge,
            Text::ScreenNotIncludedPictureTooLarge,
        ),
        (ScreenClass::NotReady, Text::ScreenNotIncludedNotReady),
        (ScreenClass::ModelCantSee, Text::ScreenNotIncludedModel),
    ] {
        let mut runs = ready();
        let luna = shown_to_luna(&mut runs);
        runs.begin_screen_hold(GestureId(1), 1, false, Some(&luna))
            .expect("held");
        // Another gesture's outcome isn't this hold's.
        assert!(!runs.hold_screen(GestureId(2), HeldScreen::NotIncluded(class)));
        assert_eq!(line(&runs, &luna), None);
        assert!(runs.hold_screen(GestureId(1), HeldScreen::NotIncluded(class)));
        assert_eq!(line(&runs, &luna), Some((text, None)), "{class:?}");
        // The first refusal stands.
        assert!(!runs.hold_screen(
            GestureId(1),
            HeldScreen::NotIncluded(ScreenClass::NothingThere)
        ));
        assert_eq!(line(&runs, &luna), Some((text, None)), "{class:?}");
    }

    // A picture that was taken leaves no line: history's icon speaks for it.
    let mut runs = ready();
    let luna = shown_to_luna(&mut runs);
    runs.begin_screen_hold(GestureId(1), 1, false, Some(&luna))
        .expect("held");
    assert!(runs.hold_screen(GestureId(1), HeldScreen::Screen));
    assert_eq!(runs.hold_screen_line(), None);
    // The brain's word lands once, whatever it was: a refusal after it changes nothing.
    assert!(!runs.hold_screen(GestureId(1), HeldScreen::NotIncluded(ScreenClass::Changed)));
    assert_eq!(runs.hold_screen_line(), None);

    // The setting switched off while the keys were down has its own words.
    runs.set_screen_holds(ScreenHolds::Off(ScreenOff::TurnedOff));
    assert!(!runs.releases_screen(GestureId(1), false, Some(&luna)));
    assert_eq!(
        line(&runs, &luna),
        Some((Text::ScreenNotIncludedTurnedOff, None))
    );

    // A hold that began as voice only has no line for an outcome to land on.
    let mut runs = ready();
    assert_eq!(
        runs.begin_screen_hold(GestureId(1), 1, false, None),
        Ok(ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: Some(ThreadId(1)),
            selection: 1,
        })
    );
    assert!(!runs.hold_screen(GestureId(1), HeldScreen::NotIncluded(ScreenClass::Changed)));
    assert_eq!(runs.hold_screen_line(), None);

    // The window flow keeps its own words for the two classes that differ.
    for (class, text) in [
        (ScreenClass::Changed, Text::ScreenNotIncludedChanged),
        (ScreenClass::TooLarge, Text::ScreenNotIncludedTooLarge),
    ] {
        let mut runs = ready();
        let luna = shown_to_luna(&mut runs);
        runs.begin_hold(GestureId(1), Some(SCREEN)).expect("held");
        assert!(runs.hold_screen(GestureId(1), HeldScreen::NotIncluded(class)));
        assert_eq!(line(&runs, &luna), Some((text, None)), "{class:?}");
    }
}

#[test]
fn a_release_the_notice_refuses_is_refused_for_good() {
    use kumochi_protocol::message::{HeldScreen, ModelName, ScreenClass, ScreenHolds, ScreenOff};
    let luna = ModelName::new("GPT-6 Luna").expect("valid");
    let sol = ModelName::new("GPT-6 Sol").expect("valid");
    let on = ScreenHolds::On {
        model: luna.clone(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: kumochi_protocol::message::ModelSlug::new("fixture").expect("valid"),
        revision: kumochi_protocol::message::NoticeRevision::FIRST.next(),
    };
    let off = |why| Some(ScreenHolds::Off(why));
    // What changed while the keys were down: (what the brain says now, the target thread has
    // its own model now, the default's name now).
    // The last column is what the hold's line then says.
    let not_ready = Some(ScreenLine::NotIncluded(ScreenClass::NotReady));
    for (now, own, default, line) in [
        (None, true, Some(&luna), not_ready),
        // The default just changed, so the stored `On` names the model before it.
        (None, false, Some(&sol), not_ready),
        (None, false, None, not_ready),
        // No icon shows on a system that can't capture, so no line does.
        (off(ScreenOff::Unsupported), false, Some(&luna), None),
        (
            off(ScreenOff::TurnedOff),
            false,
            Some(&luna),
            Some(ScreenLine::TurnedOff),
        ),
        (
            off(ScreenOff::ModelCantSee),
            false,
            Some(&luna),
            Some(ScreenLine::NotIncluded(ScreenClass::ModelCantSee)),
        ),
        (off(ScreenOff::NotReady), false, Some(&luna), not_ready),
    ] {
        // Refused by the question the shell asks first, or by the builder.
        for asked in [true, false] {
            let mut runs = ready();
            runs.set_screen_holds(on.clone());
            runs.begin_screen_hold(GestureId(1), 1, false, Some(&luna))
                .expect("held");
            if let Some(now) = now.clone() {
                runs.set_screen_holds(now);
            }
            let refused = if asked {
                !runs.releases_screen(GestureId(1), own, default)
            } else {
                runs.screen_release(GestureId(1), own, default, release())
                    .is_none()
            };
            assert!(refused, "{now:?} {own} {asked}");
            assert_eq!(runs.hold_screen_line(), line, "{now:?} {own} {asked}");
            // On again: still nothing, and the line still says why.
            runs.set_screen_holds(on.clone());
            assert!(
                !runs.releases_screen(GestureId(1), false, Some(&luna)),
                "{now:?} {own} {asked}"
            );
            assert_eq!(
                runs.screen_release(GestureId(1), false, Some(&luna), release()),
                None,
                "{now:?} {own} {asked}"
            );
            assert_eq!(runs.hold_screen_line(), line, "{now:?} {own} {asked}");
            // The brain's later word can't replace the reason the shell gave.
            assert_eq!(
                runs.hold_screen(
                    GestureId(1),
                    HeldScreen::NotIncluded(ScreenClass::NothingThere)
                ),
                line.is_none(),
                "{now:?} {own} {asked}"
            );
            // The hold itself goes on, as voice.
            assert!(state(&mut runs, 1, 1, 1, Phase::Thinking).visible);
        }
    }
}

#[test]
fn a_hold_that_ended_before_its_release_builds_no_screen_release() {
    let mut runs = ready();
    let luna = shown_to_luna(&mut runs);
    let on = Some(&luna);
    // A release before the hold is listening reaches the model as a cancel.
    runs.begin_screen_hold(GestureId(1), 1, false, on)
        .expect("held");
    runs.cancel_hold();
    assert!(!runs.releases_screen(GestureId(1), false, on));
    assert_eq!(
        runs.screen_release(GestureId(1), false, on, release()),
        None
    );
    assert_eq!(runs.take_hold_cancel(), cancel(1));
    assert_eq!(
        runs.screen_release(GestureId(1), false, on, release()),
        None
    );
    // The voice hold after it inherits nothing.
    recognized(&mut runs, 2);
    for gesture in [1, 2] {
        assert!(
            !runs.releases_screen(GestureId(gesture), false, on),
            "{gesture}"
        );
        assert_eq!(
            runs.screen_release(GestureId(gesture), false, on, release()),
            None,
            "{gesture}"
        );
    }
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(2));
    // A changed selection cancels the hold.
    runs.begin_screen_hold(GestureId(3), 1, false, on)
        .expect("held");
    assert!(runs.releases_screen(GestureId(3), false, on));
    assert!(runs.select_thread(None));
    assert_eq!(
        runs.screen_release(GestureId(3), false, on, release()),
        None
    );
    assert_eq!(runs.take_hold_cancel(), cancel(3));
    // A lost connection cancels it and takes the brain's word with it; the word alone,
    // said again on the next connection, brings nothing back.
    runs.begin_screen_hold(GestureId(4), 1, false, on)
        .expect("held");
    assert!(runs.releases_screen(GestureId(4), false, on));
    runs.disconnected();
    assert_eq!(
        runs.screen_release(GestureId(4), false, on, release()),
        None
    );
    runs.connected(2);
    runs.settings(ThreadId(1));
    shown_to_luna(&mut runs);
    assert_eq!(
        runs.screen_release(GestureId(4), false, on, release()),
        None
    );
    // A hold whose run is answering is over.
    runs.begin_screen_hold(GestureId(5), 1, false, on)
        .expect("held");
    state(&mut runs, 1, 1, 5, Phase::Thinking);
    assert!(runs.releases_screen(GestureId(5), false, on));
    state(&mut runs, 1, 1, 5, Phase::Answering);
    assert!(!runs.releases_screen(GestureId(5), false, on));
    assert_eq!(
        runs.screen_release(GestureId(5), false, on, release()),
        None
    );
}

#[test]
fn a_recognition_fence_keeps_turns_and_the_hold_and_holds_back_its_runs_late_work() {
    let mut runs = ready();
    runs.apply_state(RunId(5), ThreadId(1), Phase::Answering, None);
    runs.submit("pending typed words", |_| true)
        .expect("queued");
    recognized(&mut runs, 1);
    assert_eq!(runs.fence_run(RunId(4)), None, "not the stoppable run");
    assert_eq!(runs.fence_run(RunId(5)), stop(5));
    assert_eq!(runs.submitted.len(), 1, "the turn stays");
    assert_eq!(runs.take_hold_cancel(), None, "the hold stays");
    assert_eq!(runs.owned_disposition(RunId(5)), RunDisposition::Stopped);
    assert_eq!(runs.owned_disposition(RunId(6)), RunDisposition::Live);
    runs.apply_data(DataToShell::RunEnd { run: RunId(5) }, 4);
    runs.apply_state(RunId(5), ThreadId(1), Phase::Done, None);
    assert!(runs.pending_stops.is_empty(), "both endings retire it");
}

#[test]
fn a_run_fenced_before_its_run_state_takes_its_thread_from_it() {
    let mut runs = ready();
    recognized(&mut runs, 1);
    assert_eq!(runs.fence_run(RunId(6)), stop(6), "no RunState yet");
    assert_eq!(runs.owned_disposition(RunId(6)), RunDisposition::Stopped);
    assert_eq!(runs.owned_disposition(RunId(7)), RunDisposition::Live);
    let applied = runs.apply_state(RunId(6), ThreadId(2), Phase::Answering, None);
    assert!(!applied.visible, "its late RunState shows nothing");
    runs.apply_data(DataToShell::RunEnd { run: RunId(6) }, 4);
    assert!(runs.confirm_delete(ThreadId(2)));
    runs.deleted(ThreadId(2), Deleted::Hidden, None);
    assert!(
        runs.pending_stops.is_empty(),
        "its thread's delete closed it"
    );
}

#[test]
fn presenter_first_runs_retire_their_stops_whichever_ending_comes_first() {
    let mut runs = ready();
    for (run, end_first) in [(6, true), (7, false), (8, true), (9, false)] {
        let run = RunId(run);
        if end_first {
            runs.apply_data(DataToShell::RunEnd { run }, 4);
        }
        assert_eq!(runs.fence_run(run), stop(run.0), "no RunState yet");
        if !end_first {
            runs.apply_data(DataToShell::RunEnd { run }, 4);
        }
        assert_eq!(runs.owned_disposition(run), RunDisposition::Stopped);
        runs.apply_state(run, ThreadId(1), Phase::Done, None);
        assert!(runs.pending_stops.is_empty(), "run {} retired", run.0);
    }
}
