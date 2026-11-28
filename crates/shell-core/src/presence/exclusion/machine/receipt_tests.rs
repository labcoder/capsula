//! The receipt-driven core answered late, out of turn or never, and the calls its driver makes.

use std::{
    cell::Cell,
    fmt::Write as _,
    rc::Rc,
    time::{Duration, Instant},
};

use kumochi_protocol::message::{CaptureFor, LeaseGeneration};

use super::{Ask, Error, Exclusion, Key, LIFETIME, Op, SLOTS, Slot, Stand, Surfaces};

const TURN: CaptureFor = CaptureFor::Turn(1);
/// The acquire's bound, written out so a change to the constant shows here.
const ACQUIRE_BOUND: Duration = Duration::from_millis(250);

fn drain<R>(owner: &mut Exclusion<R>) -> Vec<Ask> {
    std::iter::from_fn(|| owner.next_ask()).collect()
}

fn ops(asks: &[Ask]) -> Vec<(Op, Slot)> {
    asks.iter().map(|ask| (ask.op, ask.slot)).collect()
}

fn each(op: Op, slots: &[Slot]) -> Vec<(Op, Slot)> {
    slots.iter().map(|&slot| (op, slot)).collect()
}

/// Answers each snapshot at `at` with a receipt naming its slot.
fn read(owner: &mut Exclusion<Slot>, asks: &[Ask], at: Instant) {
    for ask in asks {
        owner.snapshot(ask.id, Ok(Some(ask.slot)), at);
    }
}

fn confirm(owner: &mut Exclusion<Slot>, asks: &[Ask], at: Instant) {
    for ask in asks {
        owner.excluded(ask.id, Ok(()), at);
    }
}

/// Begins a lease whose `missing` slot has no surface and answers every snapshot.
fn snapshotted(owner: &mut Exclusion<Slot>, now: Instant, missing: Option<Slot>) -> Key {
    let key = owner.begin(9, TURN, now).unwrap();
    for ask in drain(owner) {
        owner.snapshot(
            ask.id,
            Ok((Some(ask.slot) != missing).then_some(ask.slot)),
            now,
        );
    }
    key
}

fn granted(owner: &mut Exclusion<Slot>, now: Instant, missing: Option<Slot>) -> Key {
    let key = snapshotted(owner, now, missing);
    let excludes = drain(owner);
    confirm(owner, &excludes, now);
    assert_eq!(owner.stand(), Stand::Granted);
    key
}

#[test]
fn nothing_is_granted_until_the_last_exclude_is_confirmed() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let key = owner.begin(9, TURN, now).unwrap();
    let snapshots = drain(&mut owner);
    assert_eq!(ops(&snapshots), each(Op::Snapshot, &SLOTS));
    read(&mut owner, &snapshots[1..], now);
    assert_eq!(
        owner.next_ask(),
        None,
        "no exclude before every snapshot is in"
    );
    read(&mut owner, &snapshots[..1], now);
    let excludes = drain(&mut owner);
    assert_eq!(ops(&excludes), each(Op::Exclude, &SLOTS));
    let (last, rest) = excludes.split_last().unwrap();
    for ask in rest {
        owner.excluded(ask.id, Ok(()), now);
        assert_eq!(owner.stand(), Stand::Acquiring);
        assert!(!owner.live(key.generation()), "an exclude is still out");
    }
    owner.excluded(last.id, Ok(()), now);
    assert_eq!(owner.stand(), Stand::Granted);
    assert!(owner.live(key.generation()), "every exclude is confirmed");
}

#[test]
fn an_exclude_unanswered_at_the_bound_restores_every_receipt_held() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let key = snapshotted(&mut owner, now, Some(Slot::Status));
    let asked: Vec<_> = std::iter::from_fn(|| owner.next_ask()).take(3).collect();
    assert_eq!(
        ops(&asked),
        each(Op::Exclude, &[Slot::Dock, Slot::Proxy, Slot::Text])
    );
    confirm(&mut owner, &asked[..2], now);
    owner.overdue(now + ACQUIRE_BOUND);
    assert_eq!(owner.stand(), Stand::Restoring);
    assert!(!owner.live(key.generation()), "a missed bound revokes");
    owner.excluded(asked[2].id, Ok(()), now + ACQUIRE_BOUND);
    assert_eq!(
        ops(&drain(&mut owner)),
        each(
            Op::Restore,
            &[
                Slot::Dock,
                Slot::Proxy,
                Slot::Text,
                Slot::Threads,
                Slot::Settings,
                Slot::Drawing,
                Slot::Onboarding,
            ]
        ),
        "the unanswered exclude and the ones never asked hold receipts too"
    );
}

#[test]
fn an_unanswered_restore_stays_busy_past_any_time_until_its_surface_is_gone() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let key = snapshotted(&mut owner, now, None);
    assert_eq!(drain(&mut owner).len(), 8);
    owner.overdue(now + ACQUIRE_BOUND);
    for ask in drain(&mut owner) {
        if ask.slot != Slot::Drawing {
            owner.restored(ask.id, Ok(()));
        }
    }
    let late = now + LIFETIME * 100;
    owner.overdue(late);
    assert!(
        owner.lapse(key, late),
        "the expired key still names the lease"
    );
    assert_eq!(owner.stand(), Stand::Restoring);
    assert_eq!(owner.begin(10, TURN, late), Err(Error::Busy));
    assert_eq!(owner.next_ask(), None, "the restore out is not asked twice");
    assert!(
        owner.gone(Slot::Drawing, |_| true, late),
        "the drawing's surface is gone"
    );
    assert_eq!(owner.stand(), Stand::Idle);
    assert_eq!(
        owner.begin(10, TURN, late).map(Key::generation),
        Ok(LeaseGeneration(2))
    );
}

#[test]
fn a_restore_confirmed_after_the_bound_frees_the_lease() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    snapshotted(&mut owner, now, None);
    assert_eq!(drain(&mut owner).len(), 8);
    owner.overdue(now + ACQUIRE_BOUND);
    let restores = drain(&mut owner);
    assert_eq!(ops(&restores), each(Op::Restore, &SLOTS));
    owner.overdue(now + LIFETIME);
    owner.revoke();
    assert_eq!(
        owner.next_ask(),
        None,
        "a restore still out is not asked again"
    );
    for ask in &restores {
        owner.restored(ask.id, Ok(()));
    }
    assert_eq!(owner.stand(), Stand::Idle);
}

#[test]
fn answers_after_a_refusal_or_a_revoke_grant_nothing() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let refused = owner.begin(9, TURN, now).unwrap();
    let snapshots = drain(&mut owner);
    owner.snapshot(snapshots[0].id, Err(Error::Surface), now);
    assert_eq!(owner.stand(), Stand::Idle);
    read(&mut owner, &snapshots[1..], now);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
    assert!(!owner.live(refused.generation()), "a refusal stays refused");

    let key = snapshotted(&mut owner, now, None);
    let excludes = drain(&mut owner);
    owner.excluded(excludes[0].id, Err(Error::Surface), now);
    assert_eq!(owner.stand(), Stand::Restoring);
    confirm(&mut owner, &excludes[1..], now);
    let restores = drain(&mut owner);
    assert_eq!(ops(&restores), each(Op::Restore, &SLOTS));
    for ask in &restores {
        owner.restored(ask.id, Ok(()));
    }
    assert_eq!(owner.stand(), Stand::Idle);
    assert!(!owner.live(key.generation()), "a revoke stays revoked");
}

#[test]
fn duplicate_stale_and_unwaited_answers_change_nothing() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    owner.begin(9, TURN, now).unwrap();
    let before = drain(&mut owner);
    owner.revoke();
    assert_eq!(owner.stand(), Stand::Idle);
    owner.begin(9, TURN, now).unwrap();
    assert_eq!(owner.begin(10, TURN, now), Err(Error::Busy));
    let snapshots = drain(&mut owner);
    read(&mut owner, &before, now);
    read(&mut owner, &snapshots[1..], now);
    owner.snapshot(snapshots[1].id, Ok(Some(Slot::Onboarding)), now);
    assert_eq!(owner.receipt(Slot::Proxy), Some(&Slot::Proxy));
    owner.excluded(snapshots[0].id, Err(Error::Surface), now);
    owner.restored(snapshots[0].id, Ok(()));
    assert_eq!(
        (owner.stand(), owner.next_ask()),
        (Stand::Acquiring, None),
        "the dock's snapshot is still out"
    );
    read(&mut owner, &snapshots[..1], now);
    assert_eq!(ops(&drain(&mut owner)), each(Op::Exclude, &SLOTS));
    assert_eq!(owner.begin(10, TURN, now), Err(Error::Busy));
}

#[test]
fn the_bound_fires_at_its_exact_instant_and_not_a_nanosecond_before() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    snapshotted(&mut owner, now, None);
    assert_eq!(drain(&mut owner).len(), 8);
    let bound = now + ACQUIRE_BOUND;
    owner.overdue(
        bound
            .checked_sub(Duration::from_nanos(1))
            .expect("fixture clock supports the instant before the bound"),
    );
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Acquiring, None));
    owner.overdue(bound);
    assert_eq!(owner.stand(), Stand::Restoring);
}

#[test]
fn a_snapshot_unanswered_at_the_bound_leaves_no_lease_and_asks_nothing() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    owner.begin(9, TURN, now).unwrap();
    let snapshots = drain(&mut owner);
    read(&mut owner, &snapshots[..4], now);
    owner.overdue(now + ACQUIRE_BOUND);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
    read(&mut owner, &snapshots[4..], now);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
}

#[test]
fn a_slot_opened_while_acquiring_granted_or_restoring() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let key = snapshotted(&mut owner, now, Some(Slot::Text));
    let excludes = drain(&mut owner);
    assert_eq!(excludes.len(), 7);
    assert_eq!(owner.opened(Slot::Text), Ok(()));
    let text = drain(&mut owner);
    assert_eq!(ops(&text), [(Op::Snapshot, Slot::Text)]);
    confirm(&mut owner, &excludes, now);
    assert_eq!(
        owner.stand(),
        Stand::Acquiring,
        "the opened slot isn't covered yet"
    );
    read(&mut owner, &text, now);
    let cover = drain(&mut owner);
    assert_eq!(ops(&cover), [(Op::Exclude, Slot::Text)]);
    confirm(&mut owner, &cover, now);
    assert_eq!(owner.stand(), Stand::Granted);

    let uncovered = |owner: &Exclusion<Slot>| (owner.stand(), owner.live(key.generation()));
    assert!(
        owner.gone(Slot::Settings, |_| true, now),
        "the settings surface is gone"
    );
    assert_eq!(owner.opened(Slot::Settings), Ok(()));
    assert_eq!(uncovered(&owner), (Stand::Acquiring, false));
    let settings = drain(&mut owner);
    assert_eq!(ops(&settings), [(Op::Snapshot, Slot::Settings)]);
    read(&mut owner, &settings, now);
    assert_eq!(uncovered(&owner), (Stand::Acquiring, false));
    let cover = drain(&mut owner);
    assert_eq!(ops(&cover), [(Op::Exclude, Slot::Settings)]);
    assert_eq!(uncovered(&owner), (Stand::Acquiring, false));
    confirm(&mut owner, &cover, now);
    assert_eq!(uncovered(&owner), (Stand::Granted, true));

    assert_eq!(owner.opened(Slot::Dock), Err(Error::Surface));
    assert_eq!(owner.stand(), Stand::Restoring);
    assert_eq!(owner.opened(Slot::Settings), Err(Error::Busy));
    assert_eq!(ops(&drain(&mut owner)), each(Op::Restore, &SLOTS));
}

#[test]
fn a_revoke_while_an_opened_slots_exclude_is_out_restores_it() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    granted(&mut owner, now, Some(Slot::Text));
    assert_eq!(owner.opened(Slot::Text), Ok(()));
    let text = drain(&mut owner);
    read(&mut owner, &text, now);
    assert_eq!(ops(&drain(&mut owner)), [(Op::Exclude, Slot::Text)]);
    owner.revoke();
    assert_eq!(ops(&drain(&mut owner)), each(Op::Restore, &SLOTS));
}

#[test]
fn an_answer_at_the_bound_counts_for_nothing_whether_or_not_the_timer_ran() {
    let now = Instant::now();
    let bound = now + ACQUIRE_BOUND;
    let before = bound
        .checked_sub(Duration::from_nanos(1))
        .expect("fixture clock supports the instant before the bound");
    for (at, stand) in [(before, Stand::Granted), (bound, Stand::Restoring)] {
        let mut owner = Exclusion::default();
        snapshotted(&mut owner, now, None);
        let excludes = drain(&mut owner);
        confirm(&mut owner, &excludes[..7], now);
        owner.excluded(excludes[7].id, Ok(()), at);
        assert_eq!(owner.stand(), stand);
        if stand == Stand::Restoring {
            assert_eq!(ops(&drain(&mut owner)), each(Op::Restore, &SLOTS));
        }
    }
    let mut owner = Exclusion::default();
    owner.begin(9, TURN, now).unwrap();
    let snapshots = drain(&mut owner);
    read(&mut owner, &snapshots[..7], now);
    read(&mut owner, &snapshots[7..], bound);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
}

#[test]
fn a_slot_reopened_while_a_snapshot_is_out_waits_for_its_own_snapshot() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    owner.begin(9, TURN, now).unwrap();
    let snapshots = drain(&mut owner);
    for ask in &snapshots[1..] {
        owner.snapshot(
            ask.id,
            Ok((ask.slot != Slot::Text).then_some(ask.slot)),
            now,
        );
    }
    assert_eq!(owner.opened(Slot::Text), Ok(()));
    let text = drain(&mut owner);
    assert_eq!(ops(&text), [(Op::Snapshot, Slot::Text)]);
    read(&mut owner, &snapshots[..1], now);
    assert_eq!(
        owner.next_ask(),
        None,
        "the opened slot's snapshot is still out"
    );
    read(&mut owner, &text, now);
    assert_eq!(ops(&drain(&mut owner)), each(Op::Exclude, &SLOTS));
}

#[test]
fn a_late_success_for_a_restore_asked_again_changes_nothing() {
    let mut owner = Exclusion::default();
    granted(&mut owner, Instant::now(), None);
    owner.revoke();
    let restores = drain(&mut owner);
    for ask in &restores {
        owner.restored(
            ask.id,
            if ask.slot == Slot::Drawing {
                Err(Error::Surface)
            } else {
                Ok(())
            },
        );
    }
    assert_eq!(owner.stand(), Stand::Debt);
    owner.revoke();
    let again = drain(&mut owner);
    assert_eq!(ops(&again), [(Op::Restore, Slot::Drawing)]);
    owner.restored(restores[6].id, Ok(()));
    assert_eq!(owner.stand(), Stand::Restoring);
    owner.restored(again[0].id, Ok(()));
    assert_eq!(owner.stand(), Stand::Idle);
}

#[test]
fn a_proof_about_an_earlier_surface_settles_nothing() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    let a = |receipt: &(Slot, u8)| *receipt == (Slot::Drawing, 1);
    let b = |receipt: &(Slot, u8)| *receipt == (Slot::Drawing, 2);
    owner.begin(9, TURN, now).unwrap();
    for ask in drain(&mut owner) {
        owner.snapshot(ask.id, Ok(Some((ask.slot, 1))), now);
    }
    for ask in drain(&mut owner) {
        owner.excluded(ask.id, Ok(()), now);
    }
    assert!(owner.gone(Slot::Drawing, a, now), "A is gone");
    assert_eq!(owner.opened(Slot::Drawing), Ok(()));
    let snapshot = drain(&mut owner);
    owner.snapshot(snapshot[0].id, Ok(Some((Slot::Drawing, 2))), now);
    let exclude = drain(&mut owner);
    owner.excluded(exclude[0].id, Ok(()), now);
    assert_eq!(owner.stand(), Stand::Granted);
    owner.revoke();
    for ask in drain(&mut owner) {
        owner.restored(
            ask.id,
            if ask.slot == Slot::Drawing {
                Err(Error::Surface)
            } else {
                Ok(())
            },
        );
    }
    assert_eq!(owner.stand(), Stand::Debt);
    assert!(
        !owner.gone(Slot::Drawing, a, now),
        "A's late notice proves nothing about B"
    );
    assert_eq!(owner.stand(), Stand::Debt);
    assert_eq!(owner.begin(10, TURN, now), Err(Error::Busy));
    assert!(
        owner.gone(Slot::Drawing, b, now),
        "B's own proof settles it"
    );
    assert_eq!(owner.stand(), Stand::Idle);
}

#[test]
fn a_proof_at_the_bound_grants_nothing_whether_or_not_the_timer_ran() {
    let now = Instant::now();
    let bound = now + ACQUIRE_BOUND;
    let before = bound
        .checked_sub(Duration::from_nanos(1))
        .expect("fixture clock supports the instant before the bound");
    for at in [before, bound] {
        let mut owner = Exclusion::default();
        let key = owner.begin(9, TURN, now).unwrap();
        for ask in drain(&mut owner) {
            let present = matches!(ask.slot, Slot::Dock | Slot::Drawing);
            owner.snapshot(ask.id, Ok(present.then_some(ask.slot)), now);
        }
        let excludes = drain(&mut owner);
        assert_eq!(
            ops(&excludes),
            each(Op::Exclude, &[Slot::Dock, Slot::Drawing])
        );
        owner.excluded(excludes[0].id, Ok(()), now);
        assert!(
            owner.gone(Slot::Drawing, |_| true, at),
            "the drawing is gone"
        );
        if at == before {
            assert_eq!(
                (owner.stand(), owner.live(key.generation())),
                (Stand::Granted, true)
            );
        } else {
            assert_eq!(
                (owner.stand(), owner.live(key.generation())),
                (Stand::Restoring, false)
            );
            assert_eq!(ops(&drain(&mut owner)), [(Op::Restore, Slot::Dock)]);
        }
    }
}

/// Records each snapshot, exclude and restore as `S`, `X` or `R` and the slot's number.
#[derive(Default)]
struct Recorder {
    calls: String,
    snapshot_fails: Option<Slot>,
    exclude_fails: Option<Slot>,
    restore_fails: Vec<Slot>,
    gone: [Rc<Cell<bool>>; 8],
}

impl Recorder {
    fn log(&mut self, op: char, slot: Slot) {
        write!(self.calls, "{op}{} ", slot as usize).unwrap();
    }

    fn take(&mut self) -> String {
        std::mem::take(&mut self.calls).trim_end().to_owned()
    }
}

impl Surfaces for Recorder {
    type Receipt = Rc<Cell<bool>>;

    fn snapshot(&mut self, slot: Slot) -> Result<Option<Self::Receipt>, Error> {
        self.log('S', slot);
        if self.snapshot_fails == Some(slot) {
            return Err(Error::Surface);
        }
        Ok(Some(Rc::clone(&self.gone[slot as usize])))
    }

    fn exclude(&mut self, slot: Slot, _: &Self::Receipt) -> Result<(), Error> {
        self.log('X', slot);
        if self.exclude_fails == Some(slot) {
            return Err(Error::Surface);
        }
        Ok(())
    }

    fn restore(&mut self, slot: Slot, _: &Self::Receipt) -> Result<(), Error> {
        self.log('R', slot);
        if self.restore_fails.contains(&slot) {
            return Err(Error::Surface);
        }
        Ok(())
    }

    fn gone(receipt: &Self::Receipt) -> bool {
        receipt.get()
    }

    fn same(&self, _: Slot, _: &Self::Receipt) -> bool {
        true
    }
}

#[test]
fn the_driver_acquires_and_cleans_up_with_the_same_calls_in_the_same_order() {
    let mut owner = Exclusion::default();
    let mut surfaces = Recorder::default();
    let key = owner
        .acquire(9, TURN, Instant::now(), &mut surfaces)
        .unwrap();
    assert!(owner.live(key.generation()), "a clean acquire is granted");
    assert_eq!(
        surfaces.take(),
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 X4 X5 X6 X7"
    );
    assert_eq!(owner.cleanup(&mut surfaces), Ok(()));
    assert_eq!(surfaces.take(), "R0 R1 R2 R3 R4 R5 R6 R7");
    assert_eq!(owner.stand(), Stand::Idle);
}

#[test]
fn the_driver_stops_at_a_failed_snapshot_and_rolls_back_a_failed_exclude() {
    const SNAPSHOT_FAILS: [&str; 8] = [
        "S0",
        "S0 S1",
        "S0 S1 S2",
        "S0 S1 S2 S3",
        "S0 S1 S2 S3 S4",
        "S0 S1 S2 S3 S4 S5",
        "S0 S1 S2 S3 S4 S5 S6",
        "S0 S1 S2 S3 S4 S5 S6 S7",
    ];
    const EXCLUDE_FAILS: [&str; 8] = [
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 X4 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 X4 X5 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 X4 X5 X6 R0 R1 R2 R3 R4 R5 R6 R7",
        "S0 S1 S2 S3 S4 S5 S6 S7 X0 X1 X2 X3 X4 X5 X6 X7 R0 R1 R2 R3 R4 R5 R6 R7",
    ];
    for (slot, (snapshot, exclude)) in SLOTS
        .into_iter()
        .zip(SNAPSHOT_FAILS.into_iter().zip(EXCLUDE_FAILS))
    {
        for (calls, surfaces) in [
            (
                snapshot,
                Recorder {
                    snapshot_fails: Some(slot),
                    ..Recorder::default()
                },
            ),
            (
                exclude,
                Recorder {
                    exclude_fails: Some(slot),
                    ..Recorder::default()
                },
            ),
        ] {
            let mut surfaces = surfaces;
            let mut owner = Exclusion::default();
            assert_eq!(
                owner.acquire(9, TURN, Instant::now(), &mut surfaces),
                Err(Error::Surface)
            );
            assert_eq!(surfaces.take(), calls);
            assert_eq!(owner.stand(), Stand::Idle);
        }
    }
}

#[test]
fn the_driver_keeps_a_failed_restore_as_debt_and_retries_only_it() {
    let mut owner = Exclusion::default();
    let mut surfaces = Recorder::default();
    owner
        .acquire(9, TURN, Instant::now(), &mut surfaces)
        .unwrap();
    surfaces.take();
    surfaces.restore_fails = vec![Slot::Proxy, Slot::Drawing];
    assert_eq!(owner.cleanup(&mut surfaces), Err(Error::Surface));
    assert_eq!(surfaces.take(), "R0 R1 R2 R3 R4 R5 R6 R7");
    assert_eq!(owner.stand(), Stand::Debt);
    assert_eq!(
        owner.acquire(10, TURN, Instant::now(), &mut surfaces),
        Err(Error::Busy)
    );
    assert_eq!(surfaces.take(), "");
    surfaces.restore_fails.clear();
    assert_eq!(owner.cleanup(&mut surfaces), Ok(()));
    assert_eq!(surfaces.take(), "R1 R6");
    assert_eq!(owner.stand(), Stand::Idle);
}

#[test]
fn the_driver_covers_a_reopened_slot_and_settles_a_gone_receipt_without_a_restore() {
    let mut owner = Exclusion::default();
    let mut surfaces = Recorder::default();
    owner
        .acquire(9, TURN, Instant::now(), &mut surfaces)
        .unwrap();
    surfaces.take();
    assert_eq!(owner.attach(Slot::Drawing, &mut surfaces), Ok(()));
    assert_eq!(surfaces.take(), "", "the same surface is already covered");
    surfaces.gone[Slot::Drawing as usize].set(true);
    surfaces.gone[Slot::Drawing as usize] = Rc::default();
    assert_eq!(owner.attach(Slot::Drawing, &mut surfaces), Ok(()));
    assert_eq!(surfaces.take(), "S6 X6");
    surfaces.gone[Slot::Drawing as usize].set(true);
    surfaces.snapshot_fails = Some(Slot::Drawing);
    assert_eq!(
        owner.attach(Slot::Drawing, &mut surfaces),
        Err(Error::Surface)
    );
    // The synchronous machine also restored the gone receipt here; both adapters answered that
    // call before any native call, so the core settles it instead.
    assert_eq!(surfaces.take(), "S6 R0 R1 R2 R3 R4 R5 R7");
    assert_eq!(owner.stand(), Stand::Idle);
}
