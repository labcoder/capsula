//! The output owner's capture-inhibit and scripted output checks.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{GestureId, MicPcm, RunId};

use super::{Claim, Denied, InhibitToken, OutputKey, OutputOwner, Retirement, State};
use crate::platform::microphone::{
    CollectionGate, Finalization, Generation, MicError, MicOwner, Mode, Running,
};
use crate::speaker::{Command, RELEASED, Snapshot};

const PATIENCE: Duration = Duration::from_secs(5);

/// Changes one field of a claim.
type Change = fn(&mut Claim);

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn state(owner: &OutputOwner) -> MutexGuard<'_, State> {
    owner.state.lock().expect("unpoisoned")
}

/// A claim, asked again only while a tail read overlapped a write.
fn claimed(owner: &OutputOwner, token: &InhibitToken, session: u64) -> Result<Claim, Denied> {
    let mut claim = owner.claim(token, session);
    until("a claim past a busy read", || {
        if claim.as_ref().err() != Some(&Denied::Busy) {
            return true;
        }
        claim = owner.claim(token, session);
        false
    });
    claim
}

/// An inhibit for `gesture`, bound to its armed microphone generation.
fn bound(owner: &OutputOwner, gesture: u32) -> (InhibitToken, CollectionGate, Generation) {
    let token = owner.acquire(GestureId(gesture)).expect("acquired");
    let gate = CollectionGate::default();
    let mic = gate.arm(GestureId(gesture)).expect("armed");
    owner.bind_mic(&token, &mic).expect("bound");
    (token, gate, mic)
}

#[test]
fn retirement_needs_proof_on_the_bound_microphone() {
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    let (token, _gate, _mic) = bound(&owner, 1);
    let (token, denied) = owner
        .retire(token, Retirement::NoMic)
        .expect_err("a microphone is bound");
    assert_eq!(denied, Denied::Inhibited, "kept");
    let claim = owner.claim(&token, 1).expect("claims");
    let reserved = owner.start_mic(&claim).expect("starts");
    let mut source = MicOwner::start_reserved_with_for_tests(
        reserved,
        Arc::new(AtomicBool::new(true)),
        |_, _, _, _| Ok(Finalization::Discarded(MicError::Unavailable)),
    )
    .expect("spawned");
    let (token, denied) = owner
        .retire(token, Retirement::Unstarted)
        .expect_err("it started");
    assert_eq!(denied, Denied::Inhibited, "kept");
    let (token, denied) = owner
        .retire(token, Retirement::Retired)
        .expect_err("not joined yet");
    assert_eq!(denied, Denied::Inhibited, "kept");
    until("joined", || source.try_receipt().is_some());
    assert!(
        owner.retire(token, Retirement::Retired).is_ok(),
        "the gate shows it retired"
    );
    assert!(owner.acquire(GestureId(2)).is_ok(), "the inhibit is free");
}

#[test]
fn a_claim_matches_every_field() {
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    let (token, _gate, _mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let copy = |claim: &Claim| Claim {
        session: claim.session,
        key: claim.key,
        owner: InhibitToken {
            serial: claim.owner.serial,
            gesture: claim.owner.gesture,
            revision: claim.owner.revision,
        },
    };
    let changed: [(&str, Change); 5] = [
        ("session", |claim| claim.session += 1),
        ("serial", |claim| claim.owner.serial += 1),
        ("revision", |claim| claim.owner.revision += 1),
        ("gesture", |claim| claim.owner.gesture = GestureId(9)),
        ("key", |claim| claim.key = OutputKey::NoOutput(99)),
    ];
    for (field, change) in changed {
        let mut stale = copy(&claim);
        change(&mut stale);
        assert_eq!(
            owner.start_mic(&stale).map(drop),
            Err(Denied::Stale),
            "{field}"
        );
    }
    assert!(owner.start_mic(&claim).is_ok(), "the claim itself starts");
    owner
        .retire(token, Retirement::Retired)
        .expect("let go unstarted, it retired");
    owner.end(1);
    owner.begin_quiet(2);
    let (token, _gate, _mic) = bound(&owner, 2);
    let current = owner.claim(&token, 2).expect("claims");
    assert_ne!(
        current.key, claim.key,
        "each session has its own no-output epoch"
    );
    let mut earlier = copy(&current);
    earlier.key = claim.key;
    assert_eq!(
        owner.start_mic(&earlier).map(drop),
        Err(Denied::Stale),
        "a key from an earlier session"
    );
}

#[test]
fn a_claim_whose_output_moved_on_neither_starts_nor_collects() {
    let alive = Arc::new(AtomicBool::new(true));
    {
        let owner = OutputOwner::default();
        let (commands, tail) = owner.scripted(1);
        let (token, _gate, mic) = bound(&owner, 1);
        let Command::Pause(quiet) = commands.try_recv().expect("pause") else {
            panic!("expected a pause");
        };
        tail.publish(&Snapshot {
            quiet,
            ..Snapshot::default()
        });
        let claim = claimed(&owner, &token, 1).expect("a paused generation that exposed nothing");
        tail.publish(&Snapshot {
            flags: RELEASED,
            ..Snapshot::default()
        });
        assert_eq!(
            owner.start_mic(&claim).map(drop),
            Err(Denied::Stale),
            "the key moved on"
        );
        assert!(mic.discard_unstarted().is_ok(), "it never started");
    }
    let owner = OutputOwner::default();
    let (commands, tail) = owner.scripted(1);
    let (token, _gate, mic) = bound(&owner, 1);
    let Command::Pause(quiet) = commands.try_recv().expect("pause") else {
        panic!("expected a pause");
    };
    tail.publish(&Snapshot {
        quiet,
        ..Snapshot::default()
    });
    let claim = claimed(&owner, &token, 1).expect("claims");
    let (release, held) = mpsc::channel::<()>();
    let source = MicOwner::start_reserved_with_for_tests(
        owner.start_mic(&claim).expect("starts"),
        Arc::clone(&alive),
        move |generation, _, running, _| {
            generation.running()?;
            running
                .try_send(Running::for_tests(generation.gesture(), 16_000))
                .map_err(|_| MicError::Unavailable)?;
            let _released = held.recv();
            Ok(Finalization::Discarded(MicError::Cancelled))
        },
    )
    .expect("spawned");
    let running = receipt(&source);
    tail.publish(&Snapshot {
        flags: RELEASED,
        ..Snapshot::default()
    });
    assert_eq!(
        owner.collect(&claim, &source, running),
        Err(Denied::Stale),
        "the key moved on"
    );
    assert_eq!(
        mic.mode(&alive, Instant::now()),
        Ok(Mode::Preparing),
        "it never collected"
    );
    release.send(()).expect("still running");
}

/// The source's running receipt, waited for no longer than the test's patience.
fn receipt(source: &MicOwner) -> Running {
    let mut receipt = None;
    until("a running receipt", || {
        receipt = source.running.try_recv().ok();
        receipt.is_some()
    });
    receipt.expect("received")
}

/// A capture that says it runs, then waits until `held` lets go.
fn parked(
    held: mpsc::Receiver<()>,
) -> impl FnOnce(
    &Generation,
    &AtomicBool,
    &async_channel::Sender<Running>,
    &async_channel::Sender<MicPcm>,
) -> Result<Finalization, MicError>
+ Send
+ 'static {
    move |generation, _, running, _| {
        generation.running_for_tests()?;
        running
            .try_send(Running::for_tests(generation.gesture(), 16_000))
            .map_err(|_| MicError::Unavailable)?;
        let _released = held.recv();
        Ok(Finalization::Discarded(MicError::Cancelled))
    }
}

#[test]
fn only_the_bound_microphone_collects_under_a_claim() {
    let alive = Arc::new(AtomicBool::new(true));
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    let (token, _gate, mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let other = CollectionGate::default()
        .arm(GestureId(1))
        .expect("another gate, the same gesture number");
    let (release, held) = mpsc::channel::<()>();
    let source = MicOwner::start_reserved_with_for_tests(
        other.reserve().expect("its own start"),
        Arc::clone(&alive),
        parked(held),
    )
    .expect("spawned");
    let running = receipt(&source);
    assert_eq!(
        owner.collect(&claim, &source, running),
        Err(Denied::Stale),
        "not the bound microphone"
    );
    assert_eq!(
        other.mode(&alive, Instant::now()),
        Ok(Mode::Preparing),
        "it never collected"
    );
    let (release_bound, held) = mpsc::channel::<()>();
    let bound_source = MicOwner::start_reserved_with_for_tests(
        owner.start_mic(&claim).expect("starts"),
        Arc::clone(&alive),
        parked(held),
    )
    .expect("spawned");
    let running = receipt(&bound_source);
    assert_eq!(owner.collect(&claim, &bound_source, running), Ok(()));
    assert_eq!(mic.mode(&alive, Instant::now()), Ok(Mode::Collecting));
    for release in [release, release_bound] {
        release.send(()).expect("still running");
    }
}

#[test]
fn queued_reconnect_refuses_a_bound_inhibit_that_did_not_survive_session_loss() {
    let owner = OutputOwner::default();
    let (token, _gate, _mic) = bound(&owner, 1);
    let refused = owner.begin_queued(
        2,
        Arc::new(|| {}),
        Instant::now() + PATIENCE,
        || panic!("an unrelated inhibit is a hard refusal, not a wait"),
        |_| panic!("no registration"),
    );
    assert_eq!(refused, None);
    owner.retire(token, Retirement::Unstarted).unwrap();
}

#[test]
fn ending_the_session_cancels_a_collecting_bound_microphone_without_retiring_it() {
    let alive = Arc::new(AtomicBool::new(true));
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    let (token, _gate, mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let (release, held) = mpsc::channel::<()>();
    let mut source = MicOwner::start_reserved_with_for_tests(
        owner.start_mic(&claim).expect("starts"),
        Arc::clone(&alive),
        parked(held),
    )
    .expect("spawned");
    let running = receipt(&source);
    assert_eq!(owner.collect(&claim, &source, running), Ok(()));
    owner.end(1);
    assert_eq!(
        mic.mode(&alive, Instant::now()),
        Err(MicError::Cancelled),
        "the bound gate is cancelled while its session permit still holds"
    );
    owner.begin_quiet(2);
    assert_eq!(
        owner.collect(&claim, &source, running),
        Err(Denied::Stale),
        "the old claim collects nothing"
    );
    assert_eq!(
        owner.acquire(GestureId(2)).map(drop),
        Err(Denied::Inhibited),
        "the inhibit outlives the session"
    );
    let (token, denied) = owner
        .retire(token, Retirement::Retired)
        .expect_err("cancelled is not retired");
    assert_eq!(denied, Denied::Inhibited);
    release.send(()).expect("still running");
    until("joined", || source.try_receipt().is_some());
    assert_eq!(owner.retire(token, Retirement::Retired), Ok(()));
    assert!(owner.acquire(GestureId(2)).is_ok(), "free once it retired");
}

#[test]
fn ending_the_session_cancels_a_reserved_microphone_that_retires_unspawned() {
    let alive = Arc::new(AtomicBool::new(true));
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    let (token, _gate, mic) = bound(&owner, 1);
    let claim = owner.claim(&token, 1).expect("claims");
    let reserved = owner.start_mic(&claim).expect("reserved");
    owner.end(1);
    assert_eq!(
        mic.mode(&alive, Instant::now()),
        Err(MicError::Cancelled),
        "the bound gate is cancelled"
    );
    owner.begin_quiet(2);
    assert_eq!(
        owner.acquire(GestureId(2)).map(drop),
        Err(Denied::Inhibited),
        "the inhibit outlives the session"
    );
    let (token, denied) = owner
        .retire(token, Retirement::Unstarted)
        .expect_err("its start was claimed");
    assert_eq!(denied, Denied::Inhibited);
    drop(reserved);
    assert_eq!(owner.retire(token, Retirement::Retired), Ok(()));
    assert!(owner.acquire(GestureId(2)).is_ok(), "free once it retired");
}

#[test]
fn a_contended_acquire_is_refused_without_waiting() {
    let owner = Arc::new(OutputOwner::default());
    let held = state(&owner);
    let (done, acquired) = mpsc::channel();
    let contender = Arc::clone(&owner);
    std::thread::spawn(move || done.send(contender.acquire(GestureId(1))));
    let refused = acquired.recv_timeout(Duration::from_secs(1));
    drop(held);
    assert_eq!(refused, Ok(Err(Denied::Busy)), "refused at once");
    assert!(owner.acquire(GestureId(2)).is_ok(), "no inhibit was taken");
}

#[test]
fn the_spoken_run_is_the_committed_one_even_once_the_device_drains() {
    let owner = OutputOwner::default();
    let (_commands, tail) = owner.scripted(1);
    assert_eq!(owner.spoken(1), None, "nothing committed");
    let now = Snapshot {
        exposed: 480,
        spoken: 5,
        ..Snapshot::default()
    };
    tail.publish(&now);
    assert_eq!(owner.spoken(1), Some(RunId(5)));
    assert_eq!(owner.spoken(2), None, "another session's");
    tail.publish(&Snapshot { played: 480, ..now });
    assert_eq!(
        owner.spoken(1),
        Some(RunId(5)),
        "a drained device isn't an ended run"
    );
    let held = state(&owner);
    assert_eq!(owner.spoken(1), None, "a busy owner is no evidence");
    drop(held);
    tail.publish(&Snapshot { spoken: 0, ..now });
    assert_eq!(owner.spoken(1), None, "cleared");
}

#[test]
fn without_an_opener_no_run_is_spoken() {
    let owner = OutputOwner::default();
    owner.begin_quiet(1);
    assert_eq!(owner.spoken(1), None);
}
