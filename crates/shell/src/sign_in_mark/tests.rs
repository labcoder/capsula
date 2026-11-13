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
    assert_eq!(mark, SignInMark::Settled);
    assert!(!mark.take(), "nothing was asked for here");
    mark.sent(1);
    assert_eq!(
        mark,
        SignInMark::Open {
            generation: 1,
            shown: false
        }
    );
}

/// Before the brain shows the wait, only a terminal event ends the mark: an answer that isn't
/// the wait (the brain may start the sign-in late) keeps it, so a dismissal still cancels it.
#[test]
fn a_dismissal_before_the_wait_shows_owes_the_cancel_once() {
    let mut mark = open(1);
    assert_eq!(mark.view(1, Shows::Other), Seen::Nothing);
    assert!(mark.take());
    assert_eq!(mark, SignInMark::CancelOwed { generation: 1 });
    assert!(!mark.take(), "a second close sends nothing");
    mark.cancel_sent();
    assert_eq!(mark, SignInMark::Settled);
}

#[test]
fn the_wait_shown_is_not_an_end() {
    let mut mark = open(1);
    assert_eq!(mark.view(1, Shows::Waiting), Seen::Nothing);
    assert_eq!(
        mark,
        SignInMark::Open {
            generation: 1,
            shown: true
        }
    );
    assert!(mark.take(), "a shown wait is still owed its cancel");
}

#[test]
fn each_terminal_event_ends_the_mark() {
    let mut left = open(1);
    left.view(1, Shows::Waiting);
    assert_eq!(left.view(1, Shows::Other), Seen::Ended, "the wait was left");
    let mut complete = open(1);
    assert_eq!(complete.view(1, Shows::Ended), Seen::Ended, "ended unshown");
    let mut failed = open(1);
    failed.view(1, Shows::Waiting);
    failed.failed();
    let mut gone = open(1);
    assert_eq!(
        gone.view(2, Shows::Other),
        Seen::Ended,
        "a new connection shows none"
    );
    for mark in [left, complete, failed, gone] {
        assert_eq!(mark, SignInMark::Settled);
    }
}

#[test]
fn a_wait_on_the_next_connection_moves_the_mark_there() {
    let mut mark = open(1);
    assert_eq!(mark.view(2, Shows::Waiting), Seen::Nothing);
    assert_eq!(
        mark,
        SignInMark::Open {
            generation: 2,
            shown: true
        }
    );
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
