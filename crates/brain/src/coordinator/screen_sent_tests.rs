//! A held image's mark stays from its turn's write until a send carrying it proves it wrote
//! nothing: the coordinator's half of that, from the grant to the run's end.

use kumochi_protocol::message::HeldScreen;

use super::*;
use crate::coordinator::LeaseId;

/// The lease `effects` grant.
fn lease_of(effects: &[Effect]) -> LeaseId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("a grant")
}

fn sent() -> Effect {
    Effect::HoldScreen {
        hold: KEY,
        outcome: HeldScreen::Attached,
    }
}

fn marked() -> Effect {
    Effect::Store(StoreOp::SentScreen { run: R })
}

fn refreshed() -> Effect {
    Effect::EventsChanged {
        hold: KEY,
        thread: T,
    }
}

/// Held image run R in task T, carrying B as a source too, with its first send granted and
/// nothing told yet.
fn granted() -> (Coordinator, LeaseId) {
    let mut core = ready_for("captured");
    core.submit_hold(prepared(&core, Some(T)), "words".into(), Some(image()))
        .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    let first = engine_key(&core, R);
    assert_eq!(starts(&core.handle(screened(first, &[T, B])).unwrap()), 1);
    let effects = sending(&mut core);
    assert_eq!(
        told(&effects),
        Vec::<&Effect>::new(),
        "nothing at the grant"
    );
    (core, lease_of(&effects))
}

const B: ThreadId = ThreadId(2);

#[test]
fn a_granted_send_that_wrote_nothing_clears_the_mark_before_the_run_is_told_ended() {
    let (mut core, lease) = granted();
    assert_eq!(core.handle(Input::SendUnsent { lease }).unwrap(), []);
    let effects = ended(&mut core, 4);
    assert_eq!(forgotten(&effects), [R]);
    assert_eq!(told(&effects), Vec::<&Effect>::new());
    let at = |wanted: fn(&Effect) -> bool| effects.iter().position(wanted).expect("present");
    assert!(
        at(|effect| matches!(effect, Effect::Store(StoreOp::ForgetScreen { .. })))
            < at(|effect| matches!(effect, Effect::Control(ControlToShell::RunState { .. }))),
        "{effects:?}"
    );
}

#[test]
fn only_a_send_that_may_have_written_marks_the_turn_s_picture_sent() {
    let (mut core, lease) = granted();
    let unsent = core.handle(Input::SendUnsent { lease }).unwrap();
    assert!(!unsent.contains(&marked()), "{unsent:?}");
    let again = lease_of(&sending(&mut core));
    assert_eq!(
        core.handle(Input::SendSettled { lease: again }).unwrap(),
        [sent(), marked(), refreshed()]
    );
}

#[test]
fn a_send_that_may_have_written_tells_once_and_no_later_send_clears_the_mark() {
    let (mut core, lease) = granted();
    assert_eq!(
        core.handle(Input::SendSettled { lease }).unwrap(),
        [sent(), marked(), refreshed()]
    );
    assert_eq!(core.handle(Input::SendSettled { lease }), Err(Stale));
    let again = lease_of(&sending(&mut core));
    assert_ne!(again, lease);
    assert_eq!(core.handle(Input::SendUnsent { lease: again }).unwrap(), []);
    let effects = ended(&mut core, 4);
    assert_eq!(forgotten(&effects), []);
    assert_eq!(told(&effects), Vec::<&Effect>::new());
}

#[test]
fn a_send_that_wrote_nothing_hands_the_picture_to_the_next_grant_without_a_clear() {
    let (mut core, lease) = granted();
    let mut effects = core.handle(Input::SendUnsent { lease }).unwrap();
    let again = sending(&mut core);
    let next = lease_of(&again);
    effects.extend(again);
    effects.extend(core.handle(Input::SendSettled { lease: next }).unwrap());
    effects.extend(ended(&mut core, 4));
    assert_eq!(forgotten(&effects), []);
    assert_eq!(withdrawals(&effects), []);
    assert_eq!(told(&effects), [&sent()]);
}

#[test]
fn a_failed_lease_s_late_settlement_settles_its_picture_once_and_nothing_else() {
    for unsent in [true, false] {
        let (mut core, lease) = granted();
        assert_eq!(
            core.handle(Input::Delete {
                thread: B,
                after_turns: 0,
                resend: false,
            })
            .unwrap()
            .iter()
            .filter(|effect| matches!(effect, Effect::Store(StoreOp::Delete { .. })))
            .count(),
            0,
            "held by the lease"
        );
        let due = core.handle(Input::SendDeadline { lease }).unwrap();
        assert_eq!(forgotten(&due), [], "a deadline proves nothing");
        assert_eq!(told(&due), Vec::<&Effect>::new());
        let settle = || {
            if unsent {
                Input::SendUnsent { lease }
            } else {
                Input::SendSettled { lease }
            }
        };
        let late = core.handle(settle()).unwrap();
        let expected = if unsent {
            vec![Effect::Store(StoreOp::ForgetScreen { run: R }), refreshed()]
        } else {
            vec![sent(), marked(), refreshed()]
        };
        assert_eq!(late, expected, "unsent: {unsent}");
        assert_eq!(core.handle(settle()).unwrap(), [], "unsent: {unsent}");
        assert!(core.transport_failed, "unsent: {unsent}");
        assert_eq!(core.leases.len(), 1, "the failed lease stays: {unsent}");
    }
}

#[test]
fn withdrawing_while_a_send_carries_the_picture_waits_for_that_send() {
    let (mut core, lease) = granted();
    assert_eq!(
        withdrawals(&core.handle(Input::ScreenHoldsOff).unwrap()),
        []
    );
    let unsent = core.handle(Input::SendUnsent { lease }).unwrap();
    assert_eq!(
        unsent[..2],
        [
            Effect::Store(StoreOp::WithdrawScreen { run: R }),
            refreshed()
        ]
    );
    assert_eq!(withdrawals(&unsent), [R]);
    assert_eq!(told(&unsent), Vec::<&Effect>::new());
    let (mut core, lease) = granted();
    assert_eq!(
        core.handle(Input::SendSettled { lease }).unwrap(),
        [sent(), marked(), refreshed()]
    );
    let mut effects = core.handle(Input::ScreenHoldsOff).unwrap();
    effects.extend(ended(&mut core, 4));
    assert_eq!(withdrawals(&effects), []);
    assert_eq!(forgotten(&effects), []);
}

#[test]
fn an_old_send_that_wrote_nothing_clears_only_its_own_stopped_run() {
    let (mut core, lease) = granted();
    let stopped = ended(&mut core, 1);
    assert_eq!(forgotten(&stopped), [], "a Stop proves nothing");
    core.handle(Input::RunStored { run: R }).unwrap();
    core.submit_hold(prepared(&core, Some(T)), "again".into(), Some(image()))
        .unwrap();
    let next = core
        .current_run()
        .expect("the next held run, carrying the same kind of picture");
    assert_ne!(next, R);
    assert_eq!(
        core.handle(Input::SendUnsent { lease }).unwrap(),
        [Effect::Store(StoreOp::ForgetScreen { run: R }), refreshed()]
    );
    assert_eq!(core.current_run(), Some(next));
}
