use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{GestureId, RunId};

use super::super::Retirement;
use super::{Active, LOCAL_QUIET, OutputOwner, Queued, RECEIPT_QUEUED};

/// A registered, acknowledged Plain pair with Talk-back on and Show Screen off.
fn speaking(deadline: Instant) -> Queued {
    let (ack, _) = mpsc::sync_channel(1);
    let mut owner = Queued::new(
        deadline,
        ack,
        Arc::new(OnceLock::new()),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    owner.ack = None;
    owner.talk_back = Some(true);
    owner.show_screen = Some(false);
    owner
}

#[test]
fn a_speaking_hold_needs_the_previous_runs_queued_receipt_not_its_local_quiet() {
    let mut owner = speaking(Instant::now() + Duration::from_secs(5));
    let proof = Arc::new(AtomicU8::new(LOCAL_QUIET));
    owner.active = Some(Active {
        id: RunId(1),
        input: None,
        proof: Arc::clone(&proof),
        delivery: None,
        locally_stopped: false,
    });
    assert!(!owner.hold_ready(), "local quiet alone admits no hold");
    proof.fetch_or(RECEIPT_QUEUED, Ordering::AcqRel);
    assert!(owner.hold_ready(), "the queued receipt does");
}

#[test]
fn a_typed_turn_cannot_reserve_a_speaking_pair_while_a_gesture_holds_the_inhibit() {
    let owner = OutputOwner::default();
    let opened = owner.begin_queued(
        1,
        Arc::new(|| {}),
        Instant::now() + Duration::from_secs(5),
        || false,
        speaking,
    );
    assert!(opened.is_some());
    let token = owner
        .acquire(GestureId(1))
        .expect("a quiet speaking pair lends the inhibit");
    assert!(
        !owner.queued_turn(1, Some(1), || true),
        "refused under the inhibit"
    );
    assert_eq!(owner.retire(token, Retirement::NoMic), Ok(()));
    assert!(
        owner.queued_turn(1, Some(1), || true),
        "free once it retired"
    );
}
