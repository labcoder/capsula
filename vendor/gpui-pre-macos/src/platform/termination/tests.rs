// Added for Kumochi under Apache-2.0: offline negotiation/lifetime tests, not AppKit proof.
use super::*;
use std::{cell::Cell, collections::VecDeque};

type Jobs = Rc<RefCell<VecDeque<MainJob>>>;

fn state() -> State {
    State {
        phase: Phase::Idle,
        last_epoch: 0,
        disposing: false,
        delivering: false,
        approved_ahead: false,
    }
}

fn control(handler: Handler) -> (Rc<Control>, Jobs) {
    let jobs = Jobs::default();
    let queue = Rc::clone(&jobs);
    (
        Control::new(
            Box::new(move |job| queue.borrow_mut().push_back(job)),
            handler,
        ),
        jobs,
    )
}

fn run_one(jobs: &Jobs) {
    let job = jobs.borrow_mut().pop_front().expect("one queued main job");
    job();
}

#[test]
fn coalescing_reserves_epoch_until_negative_reply_returns() {
    let mut state = state();
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, Some(1))
    );
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, None)
    );
    assert!(state.queue_reply(1, false));
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, None)
    );
    assert_eq!(state.begin_settlement(1, true), Some(false));
    assert_eq!(state.phase, Phase::Rejecting(1));
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateCancel, None)
    );
    assert!(!state.queue_reply(1, true));
    state.finish_settlement(1);
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, Some(2))
    );
}

#[test]
fn approval_precedes_native_settlement_and_is_not_a_new_admission() {
    let mut state = state();
    state.admit();
    assert!(state.queue_reply(1, true));
    assert_eq!(state.begin_settlement(1, true), Some(true));
    assert_eq!(state.phase, Phase::Approved);
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateNow, None)
    );
    assert_eq!(state.begin_settlement(1, true), None);
    state.finish_settlement(1);
    assert_eq!(state.phase, Phase::Approved);
}

#[test]
fn stale_work_cannot_answer_a_newer_epoch() {
    let mut state = state();
    state.admit();
    state.queue_reply(1, false);
    assert_eq!(state.begin_settlement(1, true), Some(false));
    state.finish_settlement(1);
    state.admit();
    assert!(!state.queue_reply(1, true));
    assert!(!state.queue_reply(1, false));
    assert!(state.queue_reply(2, false));
    assert_eq!(state.begin_settlement(1, true), None);
    state.finish_settlement(1);
    assert_eq!(state.phase, Phase::Queued(2, false));
    assert_eq!(state.begin_settlement(2, true), Some(false));
}

#[test]
fn disposal_overrides_queued_yes_and_keeps_no_reserved_through_return() {
    let mut state = state();
    state.admit();
    state.queue_reply(1, true);
    assert_eq!(state.dispose(), None);
    assert_eq!(state.phase, Phase::Queued(1, false));
    assert_eq!(state.begin_settlement(1, true), Some(false));
    assert_eq!(state.dispose(), None);
    assert_eq!(state.phase, Phase::Rejecting(1));
    state.finish_settlement(1);
    assert_eq!(state.phase, Phase::Disposed);
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateCancel, None)
    );
    assert!(!state.queue_reply(1, true));
}

#[test]
fn final_epoch_can_finish_but_never_wraps() {
    let mut state = state();
    state.last_epoch = u64::MAX - 1;
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, Some(u64::MAX))
    );
    assert!(state.queue_reply(u64::MAX, false));
    assert_eq!(state.begin_settlement(u64::MAX, true), Some(false));
    state.finish_settlement(u64::MAX);
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateCancel, None)
    );
    assert_eq!(state.last_epoch, u64::MAX);
}

#[test]
fn missing_application_cannot_approve() {
    let mut state = state();
    state.admit();
    state.queue_reply(1, true);
    assert_eq!(state.begin_settlement(1, false), Some(false));
    state.finish_settlement(1);
    assert_eq!(state.phase, Phase::Disposed);
}

#[test]
fn handler_and_reply_are_deferred_without_state_or_handler_borrows() {
    let owner = Rc::new(RefCell::new(Weak::<Control>::new()));
    let callback_owner = Rc::clone(&owner);
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    let (control, jobs) = control(Box::new(move |request| {
        let control = callback_owner
            .borrow()
            .upgrade()
            .expect("graphical owner lives");
        drop(
            control
                .state
                .try_borrow_mut()
                .expect("no state borrow in handler"),
        );
        drop(
            control
                .handler
                .try_borrow_mut()
                .expect("no handler borrow in callback"),
        );
        assert_eq!(control.admit(), NSApplicationTerminateReply::TerminateLater);
        request.reply(false);
        assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
        assert!(control.state.borrow().delivering);
        control.settle(1);
        assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
        callback_calls.set(callback_calls.get() + 1);
    }));
    *owner.borrow_mut() = Rc::downgrade(&control);
    assert_eq!(control.admit(), NSApplicationTerminateReply::TerminateLater);
    assert_eq!(control.admit(), NSApplicationTerminateReply::TerminateLater);
    assert_eq!(calls.get(), 0);
    assert_eq!(jobs.borrow().len(), 1);
    run_one(&jobs);
    assert_eq!(calls.get(), 1);
    assert!(!control.state.borrow().delivering);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    assert_eq!(jobs.borrow().len(), 1);
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Disposed);
}

#[test]
fn dropped_token_queues_a_negative_owner_beyond_recipient_lifetime() {
    let slot = Rc::new(RefCell::new(None));
    let recipient = Rc::clone(&slot);
    let (control, jobs) = control(Box::new(move |request| {
        *recipient.borrow_mut() = Some(request)
    }));
    control.admit();
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Pending(1));
    let request = slot.borrow_mut().take().expect("delivered token");
    assert_eq!(format!("{request:?}"), "TerminationRequest { .. }");
    drop(request);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    assert_eq!(jobs.borrow().len(), 1);
    let weak = Rc::downgrade(&control);
    drop(control);
    assert!(weak.upgrade().is_some());
    run_one(&jobs);
    assert!(weak.upgrade().is_none());
    assert_eq!(jobs.borrow().len(), 0);
}

#[test]
fn missing_recipient_rejects_and_does_not_inline_settle() {
    let (control, jobs) = control(Box::new(drop));
    control.admit();
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    assert_eq!(jobs.borrow().len(), 1);
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Disposed);
}

#[test]
fn explicit_reply_disarms_drop_and_disposal_makes_queued_yes_negative() {
    let slot = Rc::new(RefCell::new(None));
    let recipient = Rc::clone(&slot);
    let (control, jobs) = control(Box::new(move |request| {
        *recipient.borrow_mut() = Some(request)
    }));
    control.admit();
    run_one(&jobs);
    slot.borrow_mut()
        .take()
        .expect("delivered token")
        .reply(true);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, true));
    assert_eq!(jobs.borrow().len(), 1);
    control.dispose();
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    assert_eq!(jobs.borrow().len(), 1);
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Disposed);
}

#[test]
fn disposal_before_delivery_suppresses_handler_and_keeps_negative_work_alive() {
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    let (control, jobs) = control(Box::new(move |_| {
        callback_calls.set(callback_calls.get() + 1)
    }));
    control.admit();
    control.dispose();
    assert_eq!(jobs.borrow().len(), 2);
    run_one(&jobs);
    assert_eq!(calls.get(), 0);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Disposed);
    assert_eq!(jobs.borrow().len(), 0);
}

#[test]
fn disposed_token_cannot_queue_a_late_yes_or_retain_the_control() {
    let slot = Rc::new(RefCell::new(None));
    let recipient = Rc::clone(&slot);
    let (control, jobs) = control(Box::new(move |request| {
        *recipient.borrow_mut() = Some(request)
    }));
    control.admit();
    run_one(&jobs);
    control.dispose();
    run_one(&jobs);
    let request = slot.borrow_mut().take().expect("retained stale token");
    let weak = Rc::downgrade(&control);
    drop(control);
    assert!(weak.upgrade().is_none());
    request.reply(true);
    assert_eq!(jobs.borrow().len(), 0);
}

#[test]
#[cfg(panic = "unwind")]
fn panicking_handler_cannot_leave_delivery_active_or_a_queued_yes() {
    let (control, jobs) = control(Box::new(|request| {
        request.reply(true);
        panic!("termination handler fixture");
    }));
    control.admit();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(&jobs)));
    assert!(result.is_err());
    assert!(!control.state.borrow().delivering);
    assert_eq!(control.state.borrow().phase, Phase::Queued(1, false));
    assert_eq!(jobs.borrow().len(), 1);
    run_one(&jobs);
    assert_eq!(control.state.borrow().phase, Phase::Disposed);
}

#[test]
fn an_approved_quit_answers_now_without_a_queued_delivery() {
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    let (control, jobs) = control(Box::new(move |_| {
        callback_calls.set(callback_calls.get() + 1)
    }));
    TerminationApproval::new(&control).approve();
    assert_eq!(control.admit(), NSApplicationTerminateReply::TerminateNow);
    assert_eq!(jobs.borrow().len(), 0, "nothing waits on the main queue");
    assert_eq!(calls.get(), 0);
    assert_eq!(control.state.borrow().phase, Phase::Approved);
}

#[test]
fn approval_waits_for_a_queued_refusal_and_never_overrides_disposal() {
    let mut state = state();
    state.admit();
    assert!(state.queue_reply(1, false));
    state.approved_ahead = true;
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateLater, None),
        "the queued refusal keeps its request"
    );
    assert_eq!(state.begin_settlement(1, true), Some(false));
    state.finish_settlement(1);
    assert_eq!(state.phase, Phase::Idle);
    assert_eq!(
        state.admit(),
        (NSApplicationTerminateReply::TerminateNow, None)
    );

    let (control, _) = control(Box::new(drop));
    TerminationApproval::new(&control).approve();
    control.dispose();
    assert_eq!(control.admit(), NSApplicationTerminateReply::TerminateCancel);
}

#[test]
fn approval_does_not_keep_the_control_alive() {
    let (control, _) = control(Box::new(drop));
    let approval = TerminationApproval::new(&control);
    let weak = Rc::downgrade(&control);
    drop(control);
    assert!(weak.upgrade().is_none());
    approval.approve();
    assert_eq!(format!("{approval:?}"), "TerminationApproval { .. }");
}
