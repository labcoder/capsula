use kumochi_protocol::message::{GestureId, ScreenChoice};

use super::*;

fn ready() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs
}

fn recognized(runs: &mut Runs, gesture: u32) {
    let thread = runs.thread();
    let selection = runs.revision();
    assert_eq!(
        runs.begin_hold(GestureId(gesture)),
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
    assert_eq!(runs.begin_hold(GestureId(1)), Err(Refused::Busy));
    runs.refused(1, None);
    assert_eq!(runs.begin_hold(GestureId(1)), Err(Refused::Busy));
    recognized(&mut runs, 2);
    assert_eq!(runs.begin_hold(GestureId(3)), Err(Refused::Busy));
    runs.cancel_hold();
    assert_eq!(runs.begin_hold(GestureId(3)), Err(Refused::Busy));
    assert_eq!(runs.take_hold_cancel(), cancel(2));
    assert_eq!(runs.begin_hold(GestureId(1)), Err(Refused::Busy));
    recognized(&mut runs, u32::MAX);
    runs.cancel_hold();
    assert_eq!(runs.take_hold_cancel(), cancel(u32::MAX));
    assert_eq!(runs.begin_hold(GestureId(0)), Err(Refused::Busy));
    assert_eq!(runs.begin_hold(GestureId(u32::MAX)), Err(Refused::Busy));
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
    assert_eq!(runs.begin_hold(GestureId(2)), Err(Refused::Offline));
    runs.connected(2);
    runs.settings(ThreadId(1));
    assert_eq!(runs.begin_hold(GestureId(1)), Err(Refused::Busy));
    assert_eq!(state(&mut runs, 1, 1, 1, Phase::Thinking).stop, stop(1));
    assert_eq!(runs.begin_hold(GestureId(2)), Err(Refused::Busy));
    recognized(&mut runs, 3);
    assert_eq!(runs.turns_sent, 0);
    assert_eq!(runs.take_hold_cancel(), None);
}
