//! The clock against a fake device: its frequency, origin and host frequency all differ from the
//! rate and the wire's QPC frequency, so every conversion is exercised.

use std::time::Duration;

use super::{Clock, Fault, Observation, Tag, scale};

const RATE: u32 = 8_000;
/// Two position units per frame.
const FREQ: u64 = 16_000;
const ORIGIN: u64 = 777;
const QPC: u64 = 3_000_000;
/// Plenty of earlier frames still held when a write lands.
const HELD: u32 = u32::MAX;

fn clock() -> Clock {
    let mut clock = Clock::new(RATE, QPC);
    clock.begin(FREQ, ORIGIN).expect("begins");
    clock
}

fn content(segment: usize, first: u32) -> Tag {
    Tag::Content { segment, first }
}

fn write(clock: &mut Clock, intervals: &[(u64, Tag)]) {
    write_at(clock, 0, intervals);
}

/// Writes the intervals as a release that succeeded at `ms` on the service clock.
fn write_at(clock: &mut Clock, ms: u64, intervals: &[(u64, Tag)]) {
    let mut lease = clock.lease(HELD).expect("leases");
    for &(frames, tag) in intervals {
        clock.push(&mut lease, frames, tag).expect("pushes");
    }
    clock
        .commit(lease, Some(HELD), Duration::from_millis(ms))
        .expect("commits");
}

/// Frame `frame`, read at `ms` on both the device's host clock and the service clock.
fn reading(frame: u64, ms: u64) -> Observation {
    Observation {
        position: ORIGIN + frame * 2,
        frequency: FREQ,
        host_time: ms * 10_000,
        host_frequency: 10_000_000,
        precise: true,
        service: Duration::from_millis(ms),
    }
}

fn observe(clock: &mut Clock, at: Observation) -> Result<Vec<(usize, u32)>, Fault> {
    let mut played = Vec::new();
    clock.observe(at, |segment, count| played.push((segment, count)))?;
    Ok(played)
}

fn play(clock: &mut Clock, frame: u64, ms: u64) -> Result<Vec<(usize, u32)>, Fault> {
    observe(clock, reading(frame, ms))
}

fn frozen(clock: &mut Clock, fault: Fault) {
    assert_eq!(clock.fault(), Some(fault));
    assert_eq!(clock.lease(HELD).map(drop), Err(fault), "frozen for good");
    assert_eq!(play(clock, 0, 0), Err(fault));
}

fn ten_written() -> Clock {
    let mut clock = clock();
    write(&mut clock, &[(10, content(0, 0))]);
    clock
}

#[test]
fn played_counts_come_from_the_observed_frontier_not_the_write_frontier() {
    let mut clock = clock();
    write(&mut clock, &[(50, content(0, 0))]);
    assert_eq!(clock.unplayed(), 50);
    assert_eq!(play(&mut clock, 20, 10), Ok(vec![(0, 20)]));
    assert_eq!(clock.now(), 30_000, "10 ms in QPC ticks");
    assert_eq!(clock.unplayed(), 30);
    assert!(!clock.quiet());
}

#[test]
fn the_count_is_a_count_and_filler_between_segments_never_counts() {
    let mut clock = clock();
    write(
        &mut clock,
        &[(3, content(0, 0)), (100, Tag::Filler), (2, content(1, 0))],
    );
    assert_eq!(play(&mut clock, 3, 10), Ok(vec![(0, 3)]), "A:3, a count");
    assert_eq!(
        play(&mut clock, 103, 20),
        Ok(vec![]),
        "filler moves nothing"
    );
    assert_eq!(
        play(&mut clock, 104, 30),
        Ok(vec![(1, 1)]),
        "B's first frame"
    );
}

#[test]
fn quiet_waits_for_the_last_content_frame_not_the_filler_after_it() {
    let mut clock = clock();
    write(&mut clock, &[(10, content(0, 0)), (5, Tag::Filler)]);
    play(&mut clock, 9, 10).expect("plays");
    assert!(!clock.quiet(), "one frame short of the tail");
    play(&mut clock, 10, 20).expect("plays");
    assert!(clock.quiet());
}

#[test]
fn unplayed_counts_written_content_only() {
    let mut clock = clock();
    write(
        &mut clock,
        &[(10, content(0, 0)), (5, Tag::Filler), (4, content(1, 0))],
    );
    assert_eq!(clock.unplayed(), 14);
    play(&mut clock, 6, 10).expect("plays");
    assert_eq!(clock.unplayed(), 8);
    play(&mut clock, 15, 20).expect("plays");
    assert_eq!(clock.unplayed(), 4);
}

#[test]
fn an_imprecise_reading_never_counts_and_one_is_harmless() {
    let mut clock = ten_written();
    let imprecise = Observation {
        precise: false,
        ..reading(8, 10)
    };
    assert_eq!(observe(&mut clock, imprecise), Ok(vec![]));
    assert_eq!(
        play(&mut clock, 5, 20),
        Ok(vec![(0, 5)]),
        "an earlier precise frame isn't a regress"
    );
}

#[test]
fn a_position_or_time_that_goes_backward_freezes() {
    let mut clock = ten_written();
    play(&mut clock, 6, 10).expect("plays");
    assert_eq!(play(&mut clock, 5, 20), Err(Fault::Regressed));
    frozen(&mut clock, Fault::Regressed);

    let mut clock = ten_written();
    let half = Observation {
        position: ORIGIN + 13,
        ..reading(0, 10)
    };
    assert_eq!(observe(&mut clock, half), Ok(vec![(0, 6)]));
    let back = Observation {
        position: ORIGIN + 12,
        ..reading(0, 20)
    };
    assert_eq!(
        observe(&mut clock, back),
        Err(Fault::Regressed),
        "half a frame back is still back"
    );

    let mut clock = ten_written();
    play(&mut clock, 6, 20).expect("plays");
    let earlier = Observation {
        host_time: 0,
        ..reading(7, 30)
    };
    assert_eq!(observe(&mut clock, earlier), Err(Fault::Regressed));

    let mut clock = ten_written();
    let before_origin = Observation {
        position: ORIGIN - 1,
        ..reading(0, 10)
    };
    assert_eq!(observe(&mut clock, before_origin), Err(Fault::Regressed));
}

#[test]
fn a_position_past_the_write_frontier_freezes() {
    let mut clock = ten_written();
    assert_eq!(play(&mut clock, 11, 10), Err(Fault::Jumped));
    frozen(&mut clock, Fault::Jumped);
}

#[test]
fn a_position_standing_still_past_the_stall_freezes() {
    let mut clock = ten_written();
    play(&mut clock, 0, 0).expect("the first reading");
    play(&mut clock, 0, 200).expect("not past the stall yet");
    assert_eq!(play(&mut clock, 0, 201), Err(Fault::Stalled));
    frozen(&mut clock, Fault::Stalled);
}

#[test]
fn advancing_within_the_stall_keeps_the_deadline_moving() {
    let mut clock = ten_written();
    play(&mut clock, 0, 0).expect("plays");
    play(&mut clock, 1, 200).expect("plays");
    play(&mut clock, 2, 400).expect("plays");
    assert_eq!(play(&mut clock, 2, 601), Err(Fault::Stalled));
}

#[test]
fn a_repeated_device_time_cannot_hold_off_the_stall() {
    let mut clock = ten_written();
    let repeated = |ms| Observation {
        service: Duration::from_millis(ms),
        ..reading(0, 0)
    };
    observe(&mut clock, repeated(0)).expect("the first reading");
    observe(&mut clock, repeated(200)).expect("not past the stall yet");
    assert_eq!(observe(&mut clock, repeated(201)), Err(Fault::Stalled));
}

#[test]
fn readings_that_stay_imprecise_cannot_hold_off_the_stall() {
    let mut clock = ten_written();
    let imprecise = |frame, ms| Observation {
        precise: false,
        ..reading(frame, ms)
    };
    observe(&mut clock, imprecise(5, 0)).expect("ignored");
    observe(&mut clock, imprecise(10, 200)).expect("ignored");
    assert_eq!(observe(&mut clock, imprecise(10, 201)), Err(Fault::Stalled));
    assert!(!clock.quiet(), "nothing it never verified played");
}

#[test]
fn the_first_write_s_deadline_runs_from_its_release() {
    let mut slow = clock();
    play(&mut slow, 0, 0).expect("an empty epoch");
    write_at(&mut slow, 205, &[(10, content(0, 0))]);
    assert_eq!(
        play(&mut slow, 4, 210),
        Ok(vec![(0, 4)]),
        "a slow first release, then a prompt reading"
    );

    let mut late = clock();
    play(&mut late, 0, 0).expect("an empty epoch");
    write_at(&mut late, 205, &[(10, content(0, 0))]);
    assert_eq!(
        play(&mut late, 4, 406),
        Err(Fault::Stalled),
        "201 ms after the release"
    );

    let mut refilled = clock();
    write_at(&mut refilled, 0, &[(10, content(0, 0))]);
    write_at(&mut refilled, 150, &[(10, content(0, 10))]);
    assert_eq!(
        play(&mut refilled, 0, 201),
        Err(Fault::Stalled),
        "a refill while frames wait doesn't renew it"
    );
}

#[test]
fn an_advance_after_the_stall_is_refused() {
    let mut clock = ten_written();
    play(&mut clock, 0, 0).expect("the first reading");
    assert_eq!(
        play(&mut clock, 10, 201),
        Err(Fault::Stalled),
        "a late recovery doesn't count"
    );
    assert!(!clock.quiet());
}

#[test]
fn a_refill_counts_only_while_the_device_still_held_earlier_frames() {
    let mut clock = ten_written();
    let mut lease = clock.lease(HELD).expect("leases");
    clock.push(&mut lease, 5, content(0, 10)).expect("pushes");
    clock
        .commit(lease, Some(6), Duration::ZERO)
        .expect("one earlier frame was still held");

    let mut clock = ten_written();
    let mut lease = clock.lease(HELD).expect("leases");
    clock.push(&mut lease, 5, content(0, 10)).expect("pushes");
    assert_eq!(
        clock.commit(lease, Some(5), Duration::ZERO),
        Err(Fault::Gap),
        "the earlier frames were gone before these landed"
    );
    frozen(&mut clock, Fault::Gap);
}

#[test]
fn a_dropped_lease_takes_nothing() {
    let mut clock = clock();
    write(&mut clock, &[(3, content(0, 0))]);
    let mut lease = clock.lease(HELD).expect("leases");
    clock.push(&mut lease, 5, content(0, 3)).expect("pushes");
    drop(lease);
    assert_eq!(
        play(&mut clock, 4, 10),
        Err(Fault::Jumped),
        "only 3 written"
    );
}

#[test]
fn a_changed_frequency_or_a_second_epoch_ends_it() {
    let mut running = ten_written();
    let changed = Observation {
        frequency: FREQ * 2,
        ..reading(1, 10)
    };
    assert_eq!(observe(&mut running, changed), Err(Fault::Epoch));
    frozen(&mut running, Fault::Epoch);

    let mut again = clock();
    assert_eq!(again.begin(FREQ, ORIGIN + 2), Err(Fault::Epoch));
    let mut unbegun = Clock::new(RATE, QPC);
    assert_eq!(
        unbegun.lease(HELD).map(drop),
        Err(Fault::Epoch),
        "never began"
    );
    let mut zero = Clock::new(RATE, QPC);
    assert_eq!(zero.begin(0, ORIGIN), Err(Fault::Epoch));
}

#[test]
fn the_first_reading_pins_the_host_time_units() {
    let mut clock = ten_written();
    play(&mut clock, 2, 10).expect("plays");
    let finer = Observation {
        host_frequency: 20_000_000,
        ..reading(4, 15)
    };
    assert_eq!(
        observe(&mut clock, finer),
        Err(Fault::Epoch),
        "a bigger count in finer units is an earlier time"
    );
    assert_eq!(clock.unplayed(), 8, "and it counted nothing");
    assert_eq!(clock.now(), 30_000);
}

#[test]
fn a_full_ledger_freezes() {
    let mut clock = clock();
    for _ in 0..64 {
        write(&mut clock, &[(1, Tag::Filler)]);
    }
    let mut lease = clock.lease(HELD).expect("leases");
    assert_eq!(clock.push(&mut lease, 1, Tag::Filler), Err(Fault::Ledger));
    frozen(&mut clock, Fault::Ledger);
}

/// Releases `frames` frames of `tag` and commits them with `unread`.
fn release(clock: &mut Clock, frames: u64, tag: Tag, unread: Option<u32>) -> Result<(), Fault> {
    let mut lease = clock.lease(HELD).expect("leases");
    clock.push(&mut lease, frames, tag).expect("pushes");
    clock.commit(lease, unread, Duration::ZERO)
}

#[test]
fn content_the_device_took_but_the_clock_cannot_place_is_never_quiet() {
    for (unread, fault) in [(Some(5), Fault::Gap), (None, Fault::Lost)] {
        let mut clock = clock();
        release(&mut clock, 10, Tag::Filler, Some(HELD)).expect("filler first");
        assert!(clock.quiet(), "only filler so far");
        assert_eq!(release(&mut clock, 5, content(0, 0), unread), Err(fault));
        assert!(!clock.quiet(), "{fault:?}: released speech nobody saw play");
        assert_eq!(clock.unplayed(), 0, "and the ledger never took it");
        frozen(&mut clock, fault);
    }
}

#[test]
fn a_fault_after_the_last_content_frame_played_stays_quiet() {
    for (unread, fault) in [(Some(5), Fault::Gap), (None, Fault::Lost)] {
        let mut clock = ten_written();
        assert_eq!(play(&mut clock, 10, 10), Ok(vec![(0, 10)]));
        assert_eq!(release(&mut clock, 5, Tag::Filler, unread), Err(fault));
        assert!(clock.quiet(), "{fault:?}: only filler went unplaced");
    }
}

#[test]
fn a_device_that_already_ran_dry_takes_nothing_more() {
    let mut clock = ten_written();
    assert_eq!(clock.lease(0).map(drop), Err(Fault::Gap));
    assert!(!clock.quiet());
    frozen(&mut clock, Fault::Gap);
    let mut fresh = Clock::new(RATE, QPC);
    fresh.begin(FREQ, ORIGIN).expect("begins");
    assert_eq!(fresh.lease(0).map(drop), Ok(()), "nothing written yet");
}

#[test]
fn a_lease_of_no_frames_commits_nothing() {
    let mut clock = ten_written();
    let lease = clock.lease(HELD).expect("leases");
    assert_eq!(
        clock.commit(lease, Some(0), Duration::ZERO),
        Ok(()),
        "no frames, so no gap"
    );
    assert_eq!(play(&mut clock, 10, 10), Ok(vec![(0, 10)]));
    assert!(clock.quiet());
}

#[test]
fn an_advancing_position_at_a_repeated_time_is_no_advance() {
    let mut clock = ten_written();
    play(&mut clock, 0, 0).expect("the first reading");
    let frozen_time = |frame, ms| Observation {
        service: Duration::from_millis(ms),
        ..reading(frame, 0)
    };
    for (frame, ms) in [(2, 50), (4, 100), (7, 150), (10, 200)] {
        assert_eq!(observe(&mut clock, frozen_time(frame, ms)), Ok(vec![]));
    }
    assert!(!clock.quiet(), "nothing it can date played");
    assert_eq!(clock.unplayed(), 10);
    assert_eq!(
        observe(&mut clock, frozen_time(10, 201)),
        Err(Fault::Stalled),
        "and the deadline kept running"
    );
}

#[test]
fn a_position_past_the_write_frontier_freezes_even_at_a_repeated_time() {
    let mut clock = ten_written();
    play(&mut clock, 0, 0).expect("the first reading");
    let repeated = Observation {
        service: Duration::from_millis(50),
        ..reading(11, 0)
    };
    assert_eq!(observe(&mut clock, repeated), Err(Fault::Jumped));
}

#[test]
fn partial_retirement_crosses_the_ledger_s_wrap_with_exact_counts() {
    let mut clock = clock();
    for _ in 0..60 {
        write(&mut clock, &[(1, Tag::Filler)]);
    }
    assert_eq!(
        play(&mut clock, 60, 10),
        Ok(vec![]),
        "the head moves to slot 60"
    );
    write(
        &mut clock,
        &[
            (3, content(0, 0)),
            (2, Tag::Filler),
            (4, content(1, 0)),
            (1, Tag::Filler),
            (5, content(2, 0)),
            (2, content(3, 0)),
        ],
    );
    assert_eq!(play(&mut clock, 62, 20), Ok(vec![(0, 2)]));
    assert_eq!(play(&mut clock, 67, 30), Ok(vec![(0, 3), (1, 2)]));
    assert_eq!(
        play(&mut clock, 72, 40),
        Ok(vec![(1, 4), (2, 2)]),
        "slot 63 to slot 0"
    );
    assert_eq!(play(&mut clock, 77, 50), Ok(vec![(2, 5), (3, 2)]));
    assert!(clock.quiet());
    assert_eq!(clock.unplayed(), 0);
}

#[test]
fn conversions_of_huge_values_neither_overflow_nor_round_up() {
    assert_eq!(scale(u64::MAX, 2, 4), u64::MAX / 2);
    assert_eq!(scale(u64::MAX, 3, 2), u64::MAX, "saturates");
    assert_eq!(scale(u64::MAX - 1, 10_000_000, 10_000_000), u64::MAX - 1);
    assert_eq!(scale(7, 1, 0), 7, "a zero divisor counts as one");
}
