//! The receipt-driven core answered late, out of turn or never, and the calls its driver makes.

use std::{
    cell::Cell,
    fmt::Write as _,
    rc::Rc,
    time::{Duration, Instant},
};

use kumochi_protocol::message::{CaptureFor, LeaseGeneration};

use super::{BOUND, Error, Exclusion, Key, LIFETIME, Op, SLOTS, Slot, Stand, Surfaces};

const TURN: CaptureFor = CaptureFor::Turn(1);

fn drain(owner: &mut Exclusion<Slot>) -> Vec<super::Ask> {
    std::iter::from_fn(|| owner.next_ask()).collect()
}

fn ops(asks: &[super::Ask]) -> Vec<(Op, Slot)> {
    asks.iter().map(|ask| (ask.op, ask.slot)).collect()
}

fn each(op: Op, slots: &[Slot]) -> Vec<(Op, Slot)> {
    slots.iter().map(|&slot| (op, slot)).collect()
}

/// Answers each snapshot with a receipt naming its slot.
fn read(owner: &mut Exclusion<Slot>, asks: &[super::Ask]) {
    for ask in asks {
        owner.snapshot(ask.id, Ok(Some(ask.slot)));
    }
}

/// Begins a lease whose `missing` slot has no surface and answers every snapshot.
fn snapshotted(owner: &mut Exclusion<Slot>, now: Instant, missing: Option<Slot>) -> Key {
    let key = owner.begin(9, TURN, now).unwrap();
    for ask in drain(owner) {
        owner.snapshot(ask.id, Ok((Some(ask.slot) != missing).then_some(ask.slot)));
    }
    key
}

fn granted(owner: &mut Exclusion<Slot>, now: Instant, missing: Option<Slot>) -> Key {
    let key = snapshotted(owner, now, missing);
    for ask in drain(owner) {
        owner.excluded(ask.id, Ok(()));
    }
    assert_eq!(owner.stand(), Stand::Granted);
    key
}

#[test]
fn nothing_is_granted_until_the_last_exclude_is_confirmed() {
    let mut owner = Exclusion::default();
    let key = owner.begin(9, TURN, Instant::now()).unwrap();
    let snapshots = drain(&mut owner);
    assert_eq!(ops(&snapshots), each(Op::Snapshot, &SLOTS));
    read(&mut owner, &snapshots[1..]);
    assert_eq!(
        owner.next_ask(),
        None,
        "no exclude before every snapshot is in"
    );
    read(&mut owner, &snapshots[..1]);
    let excludes = drain(&mut owner);
    assert_eq!(ops(&excludes), each(Op::Exclude, &SLOTS));
    let (last, rest) = excludes.split_last().unwrap();
    for ask in rest {
        owner.excluded(ask.id, Ok(()));
        assert_eq!(owner.stand(), Stand::Acquiring);
        assert!(!owner.live(key.generation()), "an exclude is still out");
    }
    owner.excluded(last.id, Ok(()));
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
    owner.excluded(asked[0].id, Ok(()));
    owner.excluded(asked[1].id, Ok(()));
    owner.overdue(now + BOUND);
    assert_eq!(owner.stand(), Stand::Restoring);
    assert!(!owner.live(key.generation()), "a missed bound revokes");
    owner.excluded(asked[2].id, Ok(()));
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
    owner.overdue(now + BOUND);
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
    owner.gone(Slot::Drawing);
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
    owner.overdue(now + BOUND);
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
    owner.snapshot(snapshots[0].id, Err(Error::Surface));
    assert_eq!(owner.stand(), Stand::Idle);
    read(&mut owner, &snapshots[1..]);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
    assert!(!owner.live(refused.generation()), "a refusal stays refused");

    let key = snapshotted(&mut owner, now, None);
    let excludes = drain(&mut owner);
    owner.excluded(excludes[0].id, Err(Error::Surface));
    assert_eq!(owner.stand(), Stand::Restoring);
    for ask in &excludes[1..] {
        owner.excluded(ask.id, Ok(()));
    }
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
    let snapshots = drain(&mut owner);
    read(&mut owner, &before);
    read(&mut owner, &snapshots[1..]);
    read(&mut owner, &snapshots[1..2]);
    owner.excluded(snapshots[0].id, Ok(()));
    owner.restored(snapshots[0].id, Ok(()));
    assert_eq!(
        (owner.stand(), owner.next_ask()),
        (Stand::Acquiring, None),
        "the dock's snapshot is still out"
    );
    read(&mut owner, &snapshots[..1]);
    assert_eq!(ops(&drain(&mut owner)), each(Op::Exclude, &SLOTS));
}

#[test]
fn the_bound_fires_at_its_exact_instant_and_not_a_nanosecond_before() {
    let mut owner = Exclusion::default();
    let now = Instant::now();
    snapshotted(&mut owner, now, None);
    assert_eq!(drain(&mut owner).len(), 8);
    let bound = now + BOUND;
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
    read(&mut owner, &snapshots[..4]);
    owner.overdue(now + BOUND);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
    read(&mut owner, &snapshots[4..]);
    assert_eq!((owner.stand(), owner.next_ask()), (Stand::Idle, None));
}

#[test]
fn a_slot_opened_while_acquiring_granted_or_restoring() {
    let mut owner = Exclusion::default();
    let key = snapshotted(&mut owner, Instant::now(), Some(Slot::Text));
    let excludes = drain(&mut owner);
    assert_eq!(excludes.len(), 7);
    assert_eq!(owner.opened(Slot::Text), Ok(()));
    let text = drain(&mut owner);
    assert_eq!(ops(&text), [(Op::Snapshot, Slot::Text)]);
    for ask in &excludes {
        owner.excluded(ask.id, Ok(()));
    }
    assert_eq!(
        owner.stand(),
        Stand::Acquiring,
        "the opened slot isn't covered yet"
    );
    read(&mut owner, &text);
    let cover = drain(&mut owner);
    assert_eq!(ops(&cover), [(Op::Exclude, Slot::Text)]);
    owner.excluded(cover[0].id, Ok(()));
    assert_eq!(owner.stand(), Stand::Granted);

    owner.gone(Slot::Settings);
    assert_eq!(owner.opened(Slot::Settings), Ok(()));
    let settings = drain(&mut owner);
    assert_eq!(ops(&settings), [(Op::Snapshot, Slot::Settings)]);
    read(&mut owner, &settings);
    let cover = drain(&mut owner);
    assert_eq!(ops(&cover), [(Op::Exclude, Slot::Settings)]);
    assert!(
        owner.live(key.generation()),
        "a surface waiting to show leaves the grant"
    );
    owner.excluded(cover[0].id, Ok(()));
    assert_eq!(owner.stand(), Stand::Granted);

    assert_eq!(owner.opened(Slot::Dock), Err(Error::Surface));
    assert_eq!(owner.stand(), Stand::Restoring);
    assert_eq!(owner.opened(Slot::Settings), Err(Error::Busy));
    assert_eq!(ops(&drain(&mut owner)), each(Op::Restore, &SLOTS));
}

#[test]
fn a_revoke_while_an_opened_slots_exclude_is_out_restores_it() {
    let mut owner = Exclusion::default();
    granted(&mut owner, Instant::now(), Some(Slot::Text));
    assert_eq!(owner.opened(Slot::Text), Ok(()));
    let text = drain(&mut owner);
    read(&mut owner, &text);
    assert_eq!(ops(&drain(&mut owner)), [(Op::Exclude, Slot::Text)]);
    owner.revoke();
    assert_eq!(ops(&drain(&mut owner)), each(Op::Restore, &SLOTS));
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
