//! The shared owner against a fake device clock (the clock's own rules are in `clock_tests.rs`).
//! The worker's tests use these fixtures too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::message::{
    ControlToBrain, Cursor, OutputGeneration, RunId, SegmentId, SpeechPcm,
};

use super::{Data, Engine, Fault, Observation, Release, Taken};

const RATE: u32 = 8_000;
/// Two position units per frame.
const FREQ: u64 = 16_000;
const ORIGIN: u64 = 777;
const QPC: u64 = 3_000_000;
pub(crate) const GEN: OutputGeneration = OutputGeneration(5);
pub(crate) const R: RunId = RunId(7);
/// Plenty of written frames still in the device.
const HELD: u32 = u32::MAX;

/// Begun, announced, and with R named.
pub(crate) fn engine() -> Engine {
    let mut engine = unnamed();
    engine.name(R);
    engine
}

fn unnamed() -> Engine {
    revocable(Arc::default())
}

fn revocable(revoked: Arc<AtomicBool>) -> Engine {
    let mut engine = Engine::new(RATE, GEN, QPC, revoked);
    engine.begin(FREQ, ORIGIN).expect("begins");
    engine.reports.clear();
    engine
}

pub(crate) fn reports(engine: &mut Engine) -> Vec<ControlToBrain> {
    engine.reports.drain(..).collect()
}

pub(crate) fn queued(run: RunId, segment: u32) -> Data {
    Data::Queued {
        run,
        segment: SegmentId(segment),
    }
}

pub(crate) fn pcm(
    run: RunId,
    segment: u32,
    seq: u32,
    samples: Vec<i16>,
    last: Option<u32>,
) -> Data {
    Data::Pcm(SpeechPcm {
        run,
        segment: SegmentId(segment),
        output_generation: GEN,
        seq,
        samples,
        last,
    })
}

fn queue(engine: &mut Engine, segment: u32) {
    assert_eq!(engine.data(&queued(R, segment)), Ok(Taken::Kept), "queued");
}

fn feed(
    engine: &mut Engine,
    segment: u32,
    seq: u32,
    samples: Vec<i16>,
    last: Option<u32>,
) -> Result<Taken, Fault> {
    engine.data(&pcm(R, segment, seq, samples, last))
}

/// Writes the next frames as a device still holding earlier ones would, and returns them.
pub(crate) fn write(engine: &mut Engine, frames: usize) -> Vec<i16> {
    write_at(engine, frames, 0)
}

/// [`write`], released at `ms` on the service clock.
fn write_at(engine: &mut Engine, frames: usize, ms: u64) -> Vec<i16> {
    let mut out = vec![9; frames];
    engine
        .stage(&mut out, HELD)
        .expect("stages")
        .commit(Some(HELD), Duration::from_millis(ms))
        .expect("commits");
    out
}

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

pub(crate) fn play(engine: &mut Engine, frame: u64, ms: u64) -> Result<(), Fault> {
    engine.observe(reading(frame, ms))
}

pub(crate) fn progress(segment: u32, sample: u32, ms: u64) -> ControlToBrain {
    ControlToBrain::SpeechProgress {
        run: R,
        segment: SegmentId(segment),
        sample,
        t_qpc: ms * QPC / 1000,
    }
}

fn played(segment: u32, samples: u32) -> ControlToBrain {
    ControlToBrain::SegmentPlayed {
        run: R,
        segment: SegmentId(segment),
        samples,
    }
}

pub(crate) fn stopped(run: RunId, cursor: Option<(u32, u32)>) -> ControlToBrain {
    ControlToBrain::SpeechStopped {
        run,
        cursor: cursor.map(|(segment, sample)| Cursor {
            segment: SegmentId(segment),
            sample,
        }),
    }
}

const WITHDRAWN: ControlToBrain = ControlToBrain::AudioOutputUnavailable { generation: GEN };

fn engine_with(segments: u32) -> Engine {
    let mut engine = engine();
    for segment in 0..segments {
        queue(&mut engine, segment);
    }
    engine
}

/// Ten samples of segment 0, no final, all written.
fn ten_written() -> Engine {
    let mut engine = engine_with(1);
    assert_eq!(feed(&mut engine, 0, 0, vec![4; 10], None), Ok(Taken::Kept));
    write(&mut engine, 10);
    engine
}

#[test]
fn the_first_report_announces_the_output() {
    let mut engine = Engine::new(RATE, GEN, QPC, Arc::default());
    assert_eq!(
        reports(&mut engine),
        [ControlToBrain::AudioOutput {
            rate: RATE,
            generation: GEN
        }]
    );
}

#[test]
fn played_counts_come_from_the_observed_frontier_not_the_write_frontier() {
    let mut engine = engine_with(1);
    assert_eq!(feed(&mut engine, 0, 0, vec![7; 50], None), Ok(Taken::Kept));
    assert_eq!(write(&mut engine, 50), vec![7; 50]);
    assert_eq!(reports(&mut engine), []);
    play(&mut engine, 20, 10).expect("plays");
    assert_eq!(reports(&mut engine), [progress(0, 20, 10)]);
}

#[test]
fn the_cursor_is_a_count_and_filler_between_segments_never_counts() {
    let mut engine = engine_with(2);
    feed(&mut engine, 0, 0, vec![1, 2, 3], Some(3)).expect("kept");
    assert_eq!(write(&mut engine, 3), [1, 2, 3]);
    assert_eq!(write(&mut engine, 100), vec![0; 100], "nothing of B yet");
    feed(&mut engine, 1, 0, vec![4, 5], Some(2)).expect("kept");
    assert_eq!(write(&mut engine, 2), [4, 5]);
    play(&mut engine, 3, 10).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 3)], "A:3, a count");
    play(&mut engine, 103, 20).expect("plays");
    assert_eq!(reports(&mut engine), [], "filler moves nothing");
    play(&mut engine, 104, 30).expect("plays");
    assert_eq!(reports(&mut engine), [progress(1, 1, 30)]);
}

#[test]
fn zero_valued_speech_is_content() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![0; 4], Some(4)).expect("kept");
    write(&mut engine, 4);
    play(&mut engine, 4, 10).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 4)]);
}

#[test]
fn segments_join_in_one_buffer_in_order_and_count_apart() {
    let mut engine = engine_with(2);
    feed(&mut engine, 0, 0, vec![11, 12], Some(2)).expect("kept");
    feed(&mut engine, 1, 0, vec![91], Some(1)).expect("kept");
    assert_eq!(write(&mut engine, 4), [11, 12, 91, 0]);
    play(&mut engine, 2, 10).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 2)]);
    play(&mut engine, 3, 20).expect("plays");
    assert_eq!(reports(&mut engine), [played(1, 1)]);
}

#[test]
fn audio_for_a_later_segment_before_an_earlier_final_ends_the_speaker() {
    let mut engine = engine_with(2);
    feed(&mut engine, 0, 0, vec![11], None).expect("kept");
    assert_eq!(
        feed(&mut engine, 1, 0, vec![91], Some(1)),
        Err(Fault::Broken),
        "B's audio would sit in the arena before the rest of A's"
    );
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
    assert_eq!(
        engine.stage(&mut [0; 2], HELD).map(drop),
        Err(Fault::Broken)
    );
    engine.stop(R);
    assert_eq!(
        reports(&mut engine),
        [stopped(R, None)],
        "nothing was written, so nothing is heard"
    );
}

#[test]
fn a_clock_fault_withdraws_the_output_once_and_freezes_the_speaker() {
    let mut engine = ten_written();
    assert_eq!(play(&mut engine, 11, 10), Err(Fault::Jumped));
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
    assert_eq!(engine.fault(), Some(Fault::Jumped));
    assert_eq!(
        engine.fail(Fault::Lost),
        Fault::Jumped,
        "the first fault stays"
    );
    assert_eq!(reports(&mut engine), [], "withdrawn once");
    assert_eq!(
        engine.stage(&mut [0; 4], HELD).map(drop),
        Err(Fault::Jumped)
    );
    assert_eq!(feed(&mut engine, 0, 1, vec![1], None), Ok(Taken::Stale));
}

#[test]
fn a_refill_after_the_device_ran_dry_withdraws_the_output() {
    let mut engine = ten_written();
    let mut out = [0; 4];
    let staged = engine.stage(&mut out, HELD).expect("stages");
    assert_eq!(staged.commit(Some(4), Duration::ZERO), Err(Fault::Gap));
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
}

#[test]
fn a_dropped_stage_takes_nothing_back() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1, 2, 3], None).expect("kept");
    let mut out = [0; 3];
    drop(engine.stage(&mut out, HELD).expect("stages"));
    assert_eq!(out, [1, 2, 3]);
    assert_eq!(write(&mut engine, 3), [1, 2, 3], "the same samples again");
    assert_eq!(
        play(&mut engine, 4, 10),
        Err(Fault::Jumped),
        "only 3 were written"
    );
}

#[test]
fn underflow_writes_filler_and_the_count_waits_for_real_audio() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1, 2, 3, 4, 5], None).expect("kept");
    assert_eq!(write(&mut engine, 10), [1, 2, 3, 4, 5, 0, 0, 0, 0, 0]);
    play(&mut engine, 10, 10).expect("plays");
    assert_eq!(reports(&mut engine), [progress(0, 5, 10)]);
    feed(&mut engine, 0, 1, vec![6, 7], Some(7)).expect("kept");
    assert_eq!(write(&mut engine, 2), [6, 7]);
    play(&mut engine, 12, 20).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 7)]);
}

#[test]
fn a_stop_is_answered_only_once_the_last_written_content_frame_played() {
    let mut engine = ten_written();
    engine.stop(R);
    assert_eq!(
        write(&mut engine, 4),
        [0; 4],
        "nothing more of it is written"
    );
    play(&mut engine, 9, 10).expect("plays");
    assert_eq!(
        reports(&mut engine),
        [progress(0, 9, 10)],
        "one frame short of the tail: not quiet"
    );
    play(&mut engine, 10, 20).expect("plays");
    assert_eq!(reports(&mut engine), [stopped(R, Some((0, 10)))]);
    assert_eq!(
        feed(&mut engine, 0, 1, vec![1], None),
        Ok(Taken::Stale),
        "a stopped run takes no more"
    );
}

#[test]
fn a_repeated_stop_is_answered_once() {
    let mut engine = ten_written();
    play(&mut engine, 10, 10).expect("plays");
    reports(&mut engine);
    engine.stop(R);
    engine.stop(R);
    assert_eq!(reports(&mut engine), [stopped(R, Some((0, 10)))]);
    engine.stop(RunId(9));
    engine.stop(RunId(9));
    assert_eq!(reports(&mut engine), [stopped(RunId(9), None)]);
}

#[test]
fn a_pause_goes_quiet_at_the_tail_and_resume_neither_skips_nor_repeats() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, (1..=20).collect(), None).expect("kept");
    assert_eq!(write(&mut engine, 5), [1, 2, 3, 4, 5]);
    engine.pause(1);
    assert_eq!(write(&mut engine, 5), [0; 5], "paused: filler");
    play(&mut engine, 4, 10).expect("plays");
    assert_eq!(engine.quiet(), None, "one frame short of the tail");
    play(&mut engine, 5, 20).expect("plays");
    assert_eq!(engine.quiet(), Some(1));
    engine.resume(1);
    assert_eq!(
        write(&mut engine, 3),
        [6, 7, 8],
        "the first unwritten sample"
    );
}

#[test]
fn a_resume_before_quiet_withdraws_the_pause() {
    let mut engine = ten_written();
    engine.pause(2);
    engine.resume(2);
    play(&mut engine, 10, 10).expect("plays");
    assert_eq!(engine.quiet(), None);
}

#[test]
fn a_device_loss_with_an_unverified_tail_withholds_the_stop() {
    let mut engine = ten_written();
    play(&mut engine, 5, 10).expect("plays");
    engine.fail(Fault::Lost);
    assert_eq!(reports(&mut engine), [progress(0, 5, 10), WITHDRAWN]);
    engine.stop(R);
    assert_eq!(reports(&mut engine), [], "no quiet answer it can't prove");
}

#[test]
fn a_device_loss_after_the_tail_played_still_answers_the_stop() {
    let mut engine = ten_written();
    play(&mut engine, 10, 10).expect("plays");
    engine.fail(Fault::Lost);
    assert_eq!(reports(&mut engine), [progress(0, 10, 10), WITHDRAWN]);
    engine.stop(R);
    assert_eq!(reports(&mut engine), [stopped(R, Some((0, 10)))]);
}

#[test]
fn a_stop_after_a_played_empty_final_names_it_at_zero() {
    let mut engine = engine_with(2);
    feed(&mut engine, 0, 0, vec![1, 2, 3], Some(3)).expect("kept");
    feed(&mut engine, 1, 0, Vec::new(), Some(0)).expect("kept");
    write(&mut engine, 3);
    play(&mut engine, 3, 10).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 3), played(1, 0)]);
    engine.stop(R);
    assert_eq!(
        reports(&mut engine),
        [stopped(R, Some((1, 0)))],
        "B:0, which the brain accepts and whose heard point stays A:3"
    );
}

#[test]
fn a_late_final_reports_from_verified_history() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1, 2, 3], None).expect("kept");
    write(&mut engine, 3);
    play(&mut engine, 3, 10).expect("plays");
    assert_eq!(reports(&mut engine), [progress(0, 3, 10)]);
    feed(&mut engine, 0, 1, Vec::new(), Some(3)).expect("kept");
    assert_eq!(reports(&mut engine), [played(0, 3)]);
}

#[test]
fn progress_goes_out_at_most_every_250_ms_and_unread_progress_is_replaced() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1; 100], None).expect("kept");
    write(&mut engine, 100);
    play(&mut engine, 10, 0).expect("plays");
    assert_eq!(reports(&mut engine), [progress(0, 10, 0)]);
    play(&mut engine, 20, 150).expect("plays");
    assert_eq!(reports(&mut engine), [], "too soon");
    play(&mut engine, 30, 250).expect("plays");
    play(&mut engine, 35, 449).expect("plays");
    play(&mut engine, 38, 499).expect("plays");
    assert_eq!(
        reports(&mut engine),
        [progress(0, 30, 250)],
        "249 ms after it is still too soon"
    );
    play(&mut engine, 40, 500).expect("plays");
    play(&mut engine, 45, 700).expect("plays");
    play(&mut engine, 50, 750).expect("plays");
    assert_eq!(
        reports(&mut engine),
        [progress(0, 50, 750)],
        "the newer progress replaced the unread one"
    );
}

#[test]
fn reports_left_unread_past_the_bound_end_the_speaker() {
    let mut engine = engine();
    for run in 0..512 {
        engine.stop(RunId(100 + run));
    }
    assert_eq!(engine.fault(), None);
    engine.stop(RunId(612));
    assert_eq!(engine.fault(), Some(Fault::Backlog));
    assert!(engine.backlogged());
    let sent = reports(&mut engine);
    assert_eq!(sent.len(), 513);
    assert_eq!(sent[511], stopped(RunId(611), None));
    assert_eq!(sent[512], WITHDRAWN, "no receipt is dropped to make room");
}

#[test]
fn a_full_report_queue_ends_the_speaker_even_after_a_device_loss() {
    let mut engine = engine();
    engine.fail(Fault::Lost);
    for run in 0..511 {
        engine.stop(RunId(100 + run));
    }
    assert!(!engine.backlogged(), "the withdrawal and 511 answers fit");
    engine.stop(RunId(611));
    assert!(engine.backlogged(), "full, whatever faulted first");
    assert_eq!(engine.fault(), Some(Fault::Lost), "the first fault stays");
    let sent = reports(&mut engine);
    assert_eq!(sent.len(), 512);
    assert_eq!(sent[0], WITHDRAWN);
    assert_eq!(
        sent[510..],
        [stopped(RunId(609), None), stopped(RunId(610), None)],
        "in order, and nothing for the stop that found it full"
    );
}

#[test]
fn audio_comes_at_most_two_segments_ahead_but_metadata_and_empty_finals_any_time() {
    let mut engine = engine_with(256);
    assert_eq!(engine.data(&queued(R, 256)), Err(Fault::Broken));
    let mut engine = engine_with(5);
    assert_eq!(
        feed(&mut engine, 4, 0, Vec::new(), Some(0)),
        Ok(Taken::Kept)
    );
    feed(&mut engine, 0, 0, vec![1], Some(1)).expect("kept");
    feed(&mut engine, 1, 0, vec![1], Some(1)).expect("kept");
    assert_eq!(feed(&mut engine, 2, 0, vec![1], Some(1)), Ok(Taken::Kept));
    assert_eq!(feed(&mut engine, 3, 0, vec![1], None), Err(Fault::Broken));
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
}

#[test]
fn a_gap_a_repeat_data_after_the_final_or_a_wrong_total_ends_the_speaker() {
    let mut engine = engine_with(1);
    assert_eq!(
        feed(&mut engine, 0, 1, vec![1], None),
        Err(Fault::Broken),
        "gap"
    );
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1], None).expect("kept");
    assert_eq!(
        feed(&mut engine, 0, 0, vec![1], None),
        Err(Fault::Broken),
        "repeat"
    );
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1], Some(1)).expect("kept");
    assert_eq!(
        feed(&mut engine, 0, 1, vec![1], None),
        Err(Fault::Broken),
        "after final"
    );
    let mut engine = engine_with(1);
    assert_eq!(
        feed(&mut engine, 0, 0, vec![1, 2], Some(3)),
        Err(Fault::Broken),
        "total"
    );
    let mut engine = engine_with(0);
    assert_eq!(
        feed(&mut engine, 0, 0, vec![1], None),
        Err(Fault::Broken),
        "unqueued"
    );
}

fn twenty_seconds_queued() -> Engine {
    let mut engine = engine_with(1);
    for seq in 0..200 {
        assert_eq!(
            feed(&mut engine, 0, seq, vec![1; 800], None),
            Ok(Taken::Kept)
        );
    }
    engine
}

#[test]
fn twenty_seconds_unplayed_plus_one_sample_is_refused() {
    let mut engine = twenty_seconds_queued();
    assert_eq!(feed(&mut engine, 0, 200, vec![1], None), Err(Fault::Broken));
}

#[test]
fn written_content_still_counts_toward_twenty_seconds_until_it_plays() {
    let mut engine = twenty_seconds_queued();
    write(&mut engine, 800);
    assert_eq!(
        feed(&mut engine, 0, 200, vec![1], None),
        Err(Fault::Broken),
        "in the device, not yet played"
    );
    let mut engine = twenty_seconds_queued();
    write(&mut engine, 800);
    play(&mut engine, 800, 100).expect("plays");
    assert_eq!(
        feed(&mut engine, 0, 200, vec![1; 800], None),
        Ok(Taken::Kept)
    );
}

#[test]
fn a_long_segment_streamed_at_playback_pace_fits() {
    let mut engine = engine_with(1);
    let mut frame = 0;
    for seq in 0..250 {
        let last = (seq == 249).then_some(200_000);
        assert_eq!(
            feed(&mut engine, 0, seq, vec![1; 800], last),
            Ok(Taken::Kept)
        );
        write_at(&mut engine, 800, u64::from(seq) * 100);
        frame += 800;
        play(&mut engine, frame, u64::from(seq) * 100).expect("plays");
    }
    assert_eq!(
        reports(&mut engine),
        [progress(0, 197_600, 24_600), played(0, 200_000)],
        "25 s"
    );
}

#[test]
fn a_fenced_run_or_another_generation_is_stale() {
    let mut engine = engine_with(1);
    engine.stop(R);
    reports(&mut engine);
    assert_eq!(engine.data(&queued(RunId(6), 0)), Ok(Taken::Stale));
    assert_eq!(feed(&mut engine, 0, 0, vec![1], None), Ok(Taken::Stale));
    let mut engine = engine_with(1);
    let mut old = pcm(R, 0, 0, vec![1], None);
    if let Data::Pcm(pcm) = &mut old {
        pcm.output_generation = OutputGeneration(4);
    }
    assert_eq!(engine.data(&old), Ok(Taken::Stale));
}

#[test]
fn a_newer_run_waits_until_the_old_one_is_stopped_and_quiet() {
    let mut engine = ten_written();
    engine.name(RunId(8));
    let next = queued(RunId(8), 0);
    assert_eq!(engine.data(&next), Ok(Taken::Held));
    engine.stop(R);
    assert_eq!(
        engine.data(&next),
        Ok(Taken::Held),
        "its tail is still playing"
    );
    play(&mut engine, 10, 10).expect("plays");
    assert_eq!(
        reports(&mut engine),
        [progress(0, 10, 10), stopped(R, Some((0, 10)))]
    );
    assert_eq!(engine.data(&next), Ok(Taken::Kept));
}

#[test]
fn a_fully_played_run_gives_way_and_a_late_revoke_gets_its_cursor() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1, 2, 3], Some(3)).expect("kept");
    write(&mut engine, 3);
    play(&mut engine, 3, 10).expect("plays");
    reports(&mut engine);
    engine.name(RunId(8));
    assert_eq!(engine.data(&queued(RunId(8), 0)), Ok(Taken::Kept));
    engine.stop(R);
    assert_eq!(reports(&mut engine), [stopped(R, Some((0, 3)))]);
}

#[test]
fn a_run_s_frames_wait_until_control_names_it() {
    let mut engine = unnamed();
    assert_eq!(
        engine.data(&queued(R, 0)),
        Ok(Taken::Held),
        "before its RunState"
    );
    engine.name(R);
    assert_eq!(engine.data(&queued(R, 0)), Ok(Taken::Kept));
    feed(&mut engine, 0, 0, vec![3, 4], Some(2)).expect("kept");
    assert_eq!(write(&mut engine, 2), [3, 4]);
    engine.stop(RunId(9));
    reports(&mut engine);
    engine.name(RunId(9));
    assert_eq!(
        engine.data(&queued(RunId(9), 0)),
        Ok(Taken::Stale),
        "a revoke before its name fences it"
    );
}

#[test]
fn a_revoke_for_a_run_never_seen_is_answered_at_once() {
    let mut engine = engine();
    engine.stop(RunId(9));
    assert_eq!(reports(&mut engine), [stopped(RunId(9), None)]);
}

#[test]
fn released_speech_the_clock_cannot_place_withholds_the_stop_and_the_pause() {
    for (unread, fault) in [(Some(4), Fault::Gap), (None, Fault::Lost)] {
        let mut engine = engine_with(1);
        assert_eq!(write(&mut engine, 10), vec![0; 10], "filler first");
        feed(&mut engine, 0, 0, vec![5; 4], None).expect("kept");
        let mut out = [0; 4];
        let staged = engine.stage(&mut out, HELD).expect("stages");
        assert_eq!(out, [5; 4]);
        assert_eq!(
            staged.commit(unread, Duration::ZERO),
            Err(fault),
            "released, then unplaced"
        );
        assert_eq!(reports(&mut engine), [WITHDRAWN]);
        engine.pause(1);
        engine.stop(R);
        assert_eq!(
            reports(&mut engine),
            [],
            "{fault:?}: no receipt it can't prove"
        );
        assert_eq!(engine.quiet(), None);
    }
}

#[test]
fn a_gap_after_a_verified_tail_still_withholds_the_newer_speech() {
    let mut engine = engine_with(2);
    feed(&mut engine, 0, 0, vec![1, 2, 3], Some(3)).expect("kept");
    write(&mut engine, 3);
    play(&mut engine, 3, 10).expect("plays");
    assert_eq!(reports(&mut engine), [played(0, 3)]);
    feed(&mut engine, 1, 0, vec![4, 5], Some(2)).expect("kept");
    let mut out = [0; 2];
    let staged = engine.stage(&mut out, HELD).expect("stages");
    assert_eq!(staged.commit(Some(2), Duration::ZERO), Err(Fault::Gap));
    engine.stop(R);
    assert_eq!(
        reports(&mut engine),
        [WITHDRAWN],
        "A's tail played, but B's frames went out unplaced"
    );
}

#[test]
fn a_late_or_dry_device_is_caught_before_more_is_released() {
    let mut engine = ten_written();
    assert_eq!(
        play(&mut engine, 12, 10),
        Err(Fault::Jumped),
        "past what was written before the refill"
    );
    assert_eq!(
        engine.stage(&mut [0; 4], HELD).map(drop),
        Err(Fault::Jumped)
    );
    let mut engine = ten_written();
    assert_eq!(
        engine.stage(&mut [0; 4], 0).map(drop),
        Err(Fault::Gap),
        "the device already held nothing"
    );
    engine.stop(R);
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
}

#[test]
fn naming_a_newer_run_never_admits_an_older_one_it_skipped() {
    let mut engine = unnamed();
    engine.name(RunId(8));
    assert_eq!(engine.data(&queued(R, 0)), Ok(Taken::Stale));
    assert_eq!(
        feed(&mut engine, 0, 0, vec![3, 4], Some(2)),
        Ok(Taken::Stale)
    );
    assert_eq!(write(&mut engine, 2), [0, 0], "none of it plays");

    let mut engine = unnamed();
    engine.name(R);
    queue(&mut engine, 0);
    assert_eq!(
        feed(&mut engine, 0, 0, vec![3, 4], Some(2)),
        Ok(Taken::Kept)
    );
    assert_eq!(write(&mut engine, 2), [3, 4]);
}

#[test]
fn the_playing_run_finishes_after_a_newer_name_but_a_held_one_does_not_start() {
    let mut engine = engine_with(1);
    feed(&mut engine, 0, 0, vec![1, 2], None).expect("kept");
    engine.name(RunId(8));
    assert_eq!(feed(&mut engine, 0, 1, vec![3], Some(3)), Ok(Taken::Kept));
    assert_eq!(write(&mut engine, 3), [1, 2, 3]);

    let mut engine = unnamed();
    assert_eq!(engine.data(&queued(RunId(9), 0)), Ok(Taken::Held));
    engine.name(RunId(10));
    assert_eq!(
        engine.data(&queued(RunId(9), 0)),
        Ok(Taken::Stale),
        "a larger name doesn't release it"
    );
}

#[test]
fn stops_crossing_for_two_runs_are_answered_once_each() {
    let mut engine = engine();
    engine.stop(RunId(9));
    engine.stop(RunId(10));
    engine.stop(RunId(9));
    assert_eq!(
        reports(&mut engine),
        [stopped(RunId(9), None), stopped(RunId(10), None)]
    );
    engine.stop(RunId(12));
    engine.stop(RunId(11));
    assert_eq!(
        reports(&mut engine),
        [stopped(RunId(12), None), stopped(RunId(11), None)],
        "an older run's first receipt still goes out"
    );
}

#[test]
fn a_stop_is_answered_once_within_the_last_eight_answers() {
    let mut engine = engine();
    engine.stop(RunId(9));
    for run in 10..17 {
        engine.stop(RunId(run));
    }
    engine.stop(RunId(9));
    assert_eq!(
        reports(&mut engine),
        (9..17)
            .map(|run| stopped(RunId(run), None))
            .collect::<Vec<_>>(),
        "a repeat after seven other answers isn't answered"
    );
    engine.stop(RunId(17));
    engine.stop(RunId(9));
    engine.stop(RunId(8));
    assert_eq!(
        reports(&mut engine),
        [
            stopped(RunId(17), None),
            stopped(RunId(9), None),
            stopped(RunId(8), None)
        ],
        "past the horizon a repeat is answered again, and an older run's first stop still is"
    );
}

/// R's first four samples staged from eight queued, with the speaker's revocation flag.
fn staged_four() -> (Engine, Arc<AtomicBool>) {
    let revoked = Arc::new(AtomicBool::new(false));
    let mut engine = revocable(Arc::clone(&revoked));
    engine.name(R);
    queue(&mut engine, 0);
    feed(&mut engine, 0, 0, (1..=8).collect(), None).expect("kept");
    (engine, revoked)
}

#[test]
fn a_revoke_before_the_release_decision_sends_silence_and_takes_nothing() {
    let (mut engine, revoked) = staged_four();
    let mut out = [0; 4];
    let staged = engine.stage(&mut out, HELD).expect("stages");
    assert_eq!(out, [1, 2, 3, 4], "staged before the revoke");
    revoked.store(true, Ordering::Release);
    let mut kinds = Vec::new();
    staged
        .release(
            |release| {
                kinds.push(release);
                Ok(Duration::ZERO)
            },
            || Some(HELD),
        )
        .expect("released");
    assert_eq!(kinds, [Release::Silent]);
    assert_eq!(engine.queued, 8, "nothing taken");
    engine.stop(R);
    assert_eq!(
        reports(&mut engine),
        [stopped(R, None)],
        "nothing released can still play"
    );
}

#[test]
fn a_content_release_starts_the_deadline_when_it_succeeded() {
    let (mut engine, _revoked) = staged_four();
    let staged = engine.stage(&mut [0; 4], HELD).expect("stages");
    staged
        .release(|_| Ok(Duration::from_millis(300)), || Some(HELD))
        .expect("released");
    play(&mut engine, 4, 310).expect("a prompt reading after a late release");
}

#[test]
fn content_released_as_a_revoke_lands_is_owed_even_without_its_padding() {
    let (mut engine, _revoked) = staged_four();
    let staged = engine.stage(&mut [0; 4], HELD).expect("stages");
    assert_eq!(
        staged.release(|_| Ok(Duration::ZERO), || None),
        Err(Fault::Lost),
        "released, then unplaced"
    );
    assert_eq!(reports(&mut engine), [WITHDRAWN]);
    engine.stop(R);
    assert_eq!(reports(&mut engine), [], "no receipt it can't prove");
}

#[test]
fn a_revoked_speaker_stages_only_silence_but_owes_what_was_already_released() {
    let (mut engine, revoked) = staged_four();
    let mut out = [0; 4];
    let staged = engine.stage(&mut out, HELD).expect("stages");
    assert_eq!(out, [1, 2, 3, 4]);
    let mut kinds = Vec::new();
    staged
        .release(
            |release| {
                kinds.push(release);
                revoked.store(true, Ordering::Release);
                Ok(Duration::ZERO)
            },
            || Some(HELD),
        )
        .expect("in flight, so it's owed");
    assert_eq!(
        kinds,
        [Release::Content],
        "decided before the revoke landed"
    );
    assert_eq!(write(&mut engine, 4), [0; 4], "nothing more after it");
    engine.stop(R);
    assert_eq!(reports(&mut engine), [], "the released four haven't played");
    play(&mut engine, 4, 10).expect("plays");
    assert_eq!(
        reports(&mut engine),
        [progress(0, 4, 10), stopped(R, Some((0, 4)))]
    );
}
