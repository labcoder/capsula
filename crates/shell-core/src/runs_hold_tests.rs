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
    runs.hold_screen_refused();
    assert_eq!(runs.hold_screen_line(), None);
    recognized(&mut runs, 1);
    runs.hold_screen_refused();
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

#[test]
fn the_screen_holds_state_lasts_one_connection() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let on = ScreenHolds::On {
        model: ModelName::new("GPT-5.5").expect("valid"),
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
    assert_eq!(ScreenNotice::Hidden.text(), None);
    assert_eq!(
        ScreenNotice::On(&luna).text(),
        Some((Text::ScreenHoldsOn, Some(&luna)))
    );
    assert_eq!(
        ScreenNotice::ModelCantSee(&luna).text(),
        Some((Text::ScreenHoldsModelCantSee, Some(&luna)))
    );
    for (notice, text) in [
        (ScreenNotice::NotReady, Text::ScreenHoldsNotReady),
        (ScreenNotice::TurnedOff, Text::ScreenHoldsTurnedOff),
        (ScreenNotice::OwnModel, Text::ScreenHoldsOwnModel),
    ] {
        assert_eq!(notice.text(), Some((text, None)));
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
