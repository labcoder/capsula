use super::{Seen, Shows, SignInMark};

fn open(generation: u64) -> SignInMark {
    let mut mark = SignInMark::default();
    mark.sent(generation);
    mark
}

#[test]
fn only_a_queued_op_opens_the_mark() {
    let mut mark = SignInMark::default();
    assert_eq!(mark.view(1, Shows::Waiting), Seen::Nothing);
    assert!(!mark.take(), "nothing was asked for here");
    mark.sent(1);
    assert!(mark.take());
    assert_eq!(mark.view(1, Shows::Waiting), Seen::SendCancel);
    assert!(!mark.take());
}

/// Before the brain shows the wait, no answer on the asking connection ends the mark, complete or
/// refused (it may predate the op, or the brain may start the sign-in late), so a dismissal still
/// cancels it.
#[test]
fn a_dismissal_owes_the_cancel_once_with_or_without_a_visible_wait() {
    for shown in [Shows::Other, Shows::Waiting] {
        let mut mark = open(1);
        assert_eq!(mark.view(1, shown), Seen::Nothing);
        assert!(mark.take());
        assert!(!mark.take(), "a second close sends nothing");
        assert_eq!(mark.view(1, Shows::Waiting), Seen::SendCancel);
        mark.cancel_sent();
        assert_eq!(mark.view(1, Shows::Waiting), Seen::Nothing);
    }
}

#[test]
fn each_terminal_event_ends_the_mark() {
    let mut left = open(1);
    left.view(1, Shows::Waiting);
    assert_eq!(left.view(1, Shows::Other), Seen::Ended, "the wait was left");
    let mut failed = open(1);
    failed.view(1, Shows::Waiting);
    failed.failed();
    let mut gone = open(1);
    assert_eq!(
        gone.view(2, Shows::Other),
        Seen::Ended,
        "a new connection shows none"
    );
    for mark in [left, failed, gone] {
        assert_eq!(mark, SignInMark::Settled);
    }
}

#[test]
fn a_wait_on_the_next_connection_moves_the_mark_there() {
    let mut mark = open(1);
    assert_eq!(mark.view(2, Shows::Waiting), Seen::Nothing);
    assert!(mark.take());
    assert_eq!(mark.view(2, Shows::Other), Seen::Nothing);
    assert_eq!(mark.view(2, Shows::Waiting), Seen::SendCancel);
}

#[test]
fn an_owed_cancel_goes_at_the_next_wait_and_survives_until_sent() {
    let mut mark = open(1);
    assert!(mark.take());
    assert_eq!(mark, SignInMark::CancelOwed { generation: 1 });
    assert_eq!(
        mark.view(1, Shows::Other),
        Seen::Nothing,
        "a late start is still owed"
    );
    assert_eq!(mark.view(2, Shows::Waiting), Seen::SendCancel);
    assert_eq!(
        mark.view(2, Shows::Waiting),
        Seen::SendCancel,
        "still owed until sent"
    );
    mark.cancel_sent();
    assert_eq!(mark, SignInMark::Settled);
}

#[test]
fn an_owed_cancel_is_dropped_by_a_new_connection_with_no_wait_or_by_the_user() {
    let mut gone = open(1);
    gone.take();
    assert_eq!(gone.view(2, Shows::Other), Seen::Nothing);
    assert_eq!(gone, SignInMark::Settled);
    let mut steered = open(1);
    steered.take();
    steered.superseded();
    assert_eq!(steered, SignInMark::Settled);
    let mut failed = open(1);
    failed.take();
    failed.failed();
    assert_eq!(
        failed,
        SignInMark::Settled,
        "the attempt ended: nothing to cancel"
    );
}

#[test]
fn the_finish_hands_a_waiting_sign_in_over_and_keeps_an_owed_cancel() {
    let mut waiting = open(1);
    waiting.hand_over();
    assert_eq!(waiting, SignInMark::Settled);
    let mut owed = open(1);
    owed.take();
    owed.hand_over();
    assert_eq!(owed, SignInMark::CancelOwed { generation: 1 });
    let mut kept = open(1);
    kept.superseded();
    assert_eq!(
        kept,
        SignInMark::Open {
            generation: 1,
            shown: false
        },
        "only an owed cancel is the user's to drop"
    );
}

#[test]
fn only_a_settled_mark_adopts_a_waiting_sign_in() {
    let mut mark = SignInMark::default();
    mark.adopt(3);
    assert_eq!(
        mark,
        SignInMark::Open {
            generation: 3,
            shown: true
        }
    );
    let mut owed = open(1);
    owed.take();
    owed.adopt(3);
    assert_eq!(owed, SignInMark::CancelOwed { generation: 1 });
}
