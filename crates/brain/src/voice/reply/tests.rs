use kumochi_protocol::message::{
    Cursor, OutputGeneration, RunId, SceneId, SegmentId, SegmentStart, SpeechPcm, SpeechPromise,
    SpeechRetirement, StepRef,
};

use super::{Failure, Heard, Refused, Reply, Revoker, Speech};

const RATE: u32 = 24_000;
/// 20 s at 24 kHz.
const WINDOW: u64 = 480_000;

fn reply() -> Reply {
    Reply::new(RunId(7), RATE, OutputGeneration(1))
}

fn seg(n: u32) -> SegmentId {
    SegmentId(n)
}

fn step(scene: u64, index: u32) -> StepRef {
    StepRef {
        scene: SceneId(scene),
        index,
    }
}

fn pcm(segment: u32, seq: u32, samples: usize, last: Option<u32>) -> Speech {
    Speech::Pcm(SpeechPcm {
        run: RunId(7),
        segment: seg(segment),
        output_generation: OutputGeneration(1),
        seq,
        samples: vec![5; samples],
        last,
    })
}

fn queued(segment: u32) -> Speech {
    Speech::Queued {
        segment: seg(segment),
        step: None,
        start: SegmentStart::Plain,
        output_generation: OutputGeneration(1),
    }
}

fn speak(segment: u32, text: &str) -> Speech {
    Speech::Speak {
        segment: seg(segment),
        text: text.to_owned(),
    }
}

/// Runs `f` and returns what it asked for, without the stall-timer arms.
fn run(reply: &mut Reply, f: impl FnOnce(&mut Reply, &mut Vec<Speech>)) -> Vec<Speech> {
    let mut out = Vec::new();
    f(reply, &mut out);
    out.retain(|speech| !matches!(speech, Speech::Arm { .. }));
    out
}

/// Speaks segment `n` to the end with `samples` of audio, then the shell plays it.
fn speak_and_play(reply: &mut Reply, n: u32, samples: usize) {
    run(reply, |r, out| r.pcm(seg(n), vec![5; samples], out));
    run(reply, |r, out| r.done(seg(n), out));
    let total = u32::try_from(samples).expect("small");
    run(reply, |r, out| r.played(seg(n), total, out).expect("plays"));
}

#[test]
fn the_first_sentence_is_queued_spoken_and_given_the_whole_window() {
    let mut reply = reply();
    let out = run(&mut reply, |r, out| r.text("Hello there. And", out));
    assert_eq!(
        out,
        [queued(0), speak(0, "Hello there."), Speech::Credit(WINDOW)]
    );
}

#[test]
fn at_most_two_segments_are_spoken_beyond_the_playing_one() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. C. D. ", out));
    let out = run(&mut reply, |r, out| r.done(seg(0), out));
    assert_eq!(out, [pcm(0, 0, 0, Some(0)), speak(1, " B.")]);
    let out = run(&mut reply, |r, out| r.done(seg(1), out));
    assert_eq!(out, [pcm(1, 0, 0, Some(0)), speak(2, " C.")]);
    let out = run(&mut reply, |r, out| r.done(seg(2), out));
    assert_eq!(out, [pcm(2, 0, 0, Some(0))]);
    let out = run(&mut reply, |r, out| {
        r.played(seg(0), 0, out).expect("plays");
    });
    assert_eq!(out, [speak(3, " D.")]);
}

fn retirement(segment: u32, sample: u32, heard: u32) -> SpeechRetirement {
    SpeechRetirement {
        run: RunId(7),
        output: OutputGeneration(1),
        segment: seg(segment),
        retired: sample,
        heard,
        final_segment: true,
    }
}

#[test]
fn retirement_replenishes_a_full_window_without_hearing_through_a_gap() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. C. ", out));
    run(&mut reply, |r, out| {
        r.pcm(seg(0), vec![5; 480], out);
        r.done(seg(0), out);
        r.retired(retirement(0, 480, 480), out).unwrap();
    });
    for _ in 0..200 {
        run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 2_400], out));
    }
    assert_eq!(reply.unplayed, WINDOW);
    let partial = SpeechRetirement {
        final_segment: false,
        ..retirement(1, 6_000, 0)
    };
    assert_eq!(
        run(&mut reply, |r, out| r.retired(partial, out).unwrap()),
        [Speech::Credit(6_000)]
    );
    assert!(run(&mut reply, |r, out| r.retired(partial, out).unwrap()).is_empty());
    assert_eq!(reply.unplayed, WINDOW - 6_000);
    run(&mut reply, |r, out| {
        r.done(seg(1), out);
        r.pcm(seg(2), vec![5; 480], out);
        r.done(seg(2), out);
        assert_eq!(
            r.retired(retirement(2, 480, 0), out),
            Err(Refused::OutOfOrder)
        );
        let mut stale = retirement(1, 480_000, 0);
        stale.output = OutputGeneration(2);
        assert_eq!(r.retired(stale, out), Ok(()));
        assert_eq!(r.unplayed, WINDOW - 6_000 + 480);
        r.retired(retirement(1, 480_000, 0), out).unwrap();
        assert_eq!(r.retired(retirement(2, 480, 480), out), Err(Refused::Total));
        r.retired(retirement(2, 480, 0), out).unwrap();
        r.close(out);
    });
    assert_eq!(reply.unplayed, 0);
    let heard = Some(Heard {
        segment: seg(0),
        sample: 480,
        text: 2,
    });
    assert_eq!(reply.heard(), heard);
    assert!(reply.is_final());
    run(&mut reply, |r, out| r.revoke(Revoker::Shell, out));
    assert!(!reply.is_final());
    assert_eq!(
        reply.stopped(Some(Cursor {
            segment: seg(2),
            sample: 480
        })),
        Err(Refused::Total)
    );
    assert_eq!(
        reply.stopped(Some(Cursor {
            segment: seg(0),
            sample: 480
        })),
        Ok(())
    );
    assert_eq!(reply.heard(), heard);
    assert!(reply.is_final());
}

#[test]
fn producer_drop_does_not_refund_queued_retirement_twice() {
    let mut reply = reply();
    run(&mut reply, |r, out| {
        r.say(step(7, 0), "line", out);
        r.pcm(seg(0), vec![5; 480], out);
        let partial = SpeechRetirement {
            final_segment: false,
            ..retirement(0, 240, 240)
        };
        r.retired(partial, out).unwrap();
        assert_eq!(r.unplayed, 240);
        r.step_failed(SceneId(7), out);
        assert_eq!(r.unplayed, 0);
        let final_receipt = retirement(0, 480, 240);
        r.retired(final_receipt, out).unwrap();
        r.retired(final_receipt, out).unwrap();
    });
    assert_eq!(reply.unplayed, 0);
    assert_eq!(reply.heard().map(|heard| heard.sample), Some(240));
}

#[test]
fn credit_stops_at_the_window_and_progress_gives_it_back_in_part() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Long. ", out));
    let full = usize::try_from(WINDOW).expect("fits");
    let out = run(&mut reply, |r, out| r.pcm(seg(0), vec![5; full], out));
    assert_eq!(out.len(), 200, "2,400-sample chunks");
    assert_eq!(out[199], pcm(0, 199, 2_400, None));
    let out = run(&mut reply, |r, out| {
        r.progress(seg(0), 1_000, out).expect("plays");
    });
    assert_eq!(out, [Speech::Credit(1_000)]);
    let out = run(&mut reply, |r, out| {
        r.progress(seg(0), 1_000, out).expect("repeat");
    });
    assert_eq!(out, []);
    let out = run(&mut reply, |r, out| {
        r.progress(seg(0), 500, out).expect("stale");
    });
    assert_eq!(out, []);
}

#[test]
fn a_30_second_segment_streams_through_the_20_second_window_on_progress_alone() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Long. ", out));
    let mut sent = 0u32;
    let mut played = 0u32;
    while sent < 720_000 {
        run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 2_400], out));
        sent += 2_400;
        if u64::from(sent - played) >= WINDOW {
            played += 24_000;
            let out = run(&mut reply, |r, out| {
                r.progress(seg(0), played, out).expect("plays");
            });
            assert_eq!(out, [Speech::Credit(24_000)]);
        }
    }
    let out = run(&mut reply, |r, out| r.done(seg(0), out));
    assert_eq!(out, [pcm(0, 300, 0, Some(720_000))]);
}

#[test]
fn a_large_frame_is_split_into_100_ms_chunks() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Hi. ", out));
    let out = run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 5_000], out));
    assert_eq!(
        out,
        [
            pcm(0, 0, 2_400, None),
            pcm(0, 1, 2_400, None),
            pcm(0, 2, 200, None),
        ]
    );
    let out = run(&mut reply, |r, out| r.pcm(seg(0), Vec::new(), out));
    assert_eq!(out, []);
}

#[test]
fn the_final_chunk_is_empty_and_carries_the_samples_sent() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Hi. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 300], out));
    assert_eq!(
        run(&mut reply, |r, out| r.done(seg(0), out)),
        [pcm(0, 1, 0, Some(300))]
    );
    assert_eq!(run(&mut reply, |r, out| r.done(seg(0), out)), []);
    assert_eq!(run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 9], out)), []);
}

#[test]
fn the_run_is_final_only_once_the_last_segment_played() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("One. Two. Three", out));
    run(&mut reply, super::Reply::close);
    assert!(!reply.is_final());
    for n in 0..3 {
        run(&mut reply, |r, out| r.pcm(seg(n), vec![5; 10], out));
        run(&mut reply, |r, out| r.done(seg(n), out));
    }
    run(&mut reply, |r, out| {
        r.played(seg(0), 10, out).expect("plays");
    });
    run(&mut reply, |r, out| {
        r.played(seg(1), 10, out).expect("plays");
    });
    assert!(
        !reply.is_final(),
        "the shell's queue can run dry before the last segment plays"
    );
    run(&mut reply, |r, out| {
        r.played(seg(2), 10, out).expect("plays");
    });
    assert!(reply.is_final());
}

#[test]
fn a_late_final_chunk_holds_finality_until_it_plays() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("One. Two.", out));
    run(&mut reply, super::Reply::close);
    speak_and_play(&mut reply, 0, 10);
    run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 10], out));
    assert_eq!(
        reply.played(seg(1), 10, &mut Vec::new()),
        Err(Refused::OutOfOrder)
    );
    assert!(!reply.is_final());
    run(&mut reply, |r, out| r.done(seg(1), out));
    assert!(!reply.is_final());
    run(&mut reply, |r, out| {
        r.played(seg(1), 10, out).expect("plays");
    });
    assert!(reply.is_final());
}

#[test]
fn no_segments_are_final_once_the_producer_closes() {
    let mut reply = reply();
    assert!(!reply.is_final());
    let out = run(&mut reply, super::Reply::close);
    assert_eq!(out, []);
    assert!(reply.is_final());
}

#[test]
fn a_zero_output_segment_completes_with_zero_played_and_no_heard_point() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Hm.", out));
    run(&mut reply, super::Reply::close);
    assert_eq!(
        run(&mut reply, |r, out| r.done(seg(0), out)),
        [pcm(0, 0, 0, Some(0))]
    );
    run(&mut reply, |r, out| {
        r.played(seg(0), 0, out).expect("plays");
    });
    assert!(reply.is_final());
    assert_eq!(reply.heard(), None);
}

#[test]
fn a_tts_failure_that_stops_ends_every_open_segment_empty() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. C.", out));
    run(&mut reply, super::Reply::close);
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 40], out));
    let out = run(&mut reply, |r, out| r.failed(seg(0), Failure::Run, out));
    assert_eq!(
        out,
        [
            pcm(0, 1, 0, Some(40)),
            pcm(1, 0, 0, Some(0)),
            pcm(2, 0, 0, Some(0))
        ]
    );
    assert_eq!(run(&mut reply, |r, out| r.text(" D. E.", out)), []);
    for (n, total) in [(0, 40), (1, 0), (2, 0)] {
        run(&mut reply, |r, out| {
            r.played(seg(n), total, out).expect("plays");
        });
    }
    assert!(reply.is_final());
}

#[test]
fn a_failure_of_one_segment_moves_on_to_the_next() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. ", out));
    let out = run(&mut reply, |r, out| r.failed(seg(0), Failure::Segment, out));
    assert_eq!(out, [pcm(0, 0, 0, Some(0)), speak(1, " B.")]);
    assert_eq!(
        run(&mut reply, |r, out| r.failed(seg(0), Failure::Segment, out)),
        []
    );
}

#[test]
fn a_reservation_promises_only_an_announced_segment_even_if_synthesis_stopped() {
    let step = step(4, 0);
    for revoked in [false, true] {
        let mut reply = reply();
        run(&mut reply, |r, out| r.text("First. ", out));
        let mut out = Vec::new();
        let reserved = reply
            .reserve_say("Look here.", &mut out)
            .expect("reserved while open");
        assert_eq!(reserved, seg(1));
        assert!(out.is_empty());
        if revoked {
            run(&mut reply, |r, out| r.revoke(Revoker::Brain, out));
        } else {
            run(&mut reply, |r, out| r.failed(seg(0), Failure::Run, out));
        }
        let start = SegmentStart::Speech;
        let output_generation = OutputGeneration(1);
        let promise = reply.publish_say(reserved, step, start, "Look here.", &mut out);
        if revoked {
            assert_eq!(promise, None);
            assert_eq!(out, []);
        } else {
            assert_eq!(
                promise,
                Some(SpeechPromise {
                    output_generation,
                    segment: seg(1)
                })
            );
            assert_eq!(
                out,
                [
                    Speech::Queued {
                        segment: seg(1),
                        step: Some(step),
                        start,
                        output_generation
                    },
                    pcm(1, 0, 0, Some(0)),
                ]
            );
        }
    }
}

#[test]
fn a_failed_scene_drops_its_say_and_the_text_after_it_still_plays() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Look", out));
    let out = run(&mut reply, |r, out| r.say(step(4, 0), "A circle.", out));
    assert_eq!(
        out,
        [
            queued(0),
            Speech::Queued {
                segment: seg(1),
                step: Some(step(4, 0)),
                start: SegmentStart::Speech,
                output_generation: OutputGeneration(1),
            },
            speak(0, "Look"),
            Speech::Credit(WINDOW),
        ]
    );
    run(&mut reply, |r, out| r.done(seg(0), out));
    run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 50], out));
    run(&mut reply, |r, out| r.text(" Then text. ", out));
    let out = run(&mut reply, |r, out| r.step_failed(SceneId(4), out));
    assert_eq!(
        out,
        [
            Speech::Cancel,
            pcm(1, 1, 0, Some(50)),
            speak(2, " Then text."),
            Speech::Credit(50),
        ]
    );
    run(&mut reply, super::Reply::close);
    run(&mut reply, |r, out| {
        r.played(seg(0), 0, out).expect("plays");
    });
    assert_eq!(reply.progress(seg(1), 10, &mut Vec::new()), Ok(()));
    run(&mut reply, |r, out| r.done(seg(2), out));
    run(&mut reply, |r, out| {
        r.played(seg(2), 0, out).expect("plays");
    });
    assert!(reply.is_final(), "a dropped segment needs no SegmentPlayed");
}

#[test]
fn a_failed_scene_s_queued_says_end_and_the_next_text_speaks() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.say(step(1, 0), "One.", out));
    run(&mut reply, |r, out| r.say(step(1, 1), "Two.", out));
    run(&mut reply, |r, out| r.say(step(1, 2), "Three.", out));
    run(&mut reply, |r, out| r.text("After. ", out));
    let out = run(&mut reply, |r, out| r.step_failed(SceneId(1), out));
    assert_eq!(
        out,
        [
            Speech::Cancel,
            pcm(0, 0, 0, Some(0)),
            pcm(1, 0, 0, Some(0)),
            pcm(2, 0, 0, Some(0)),
            speak(3, "After."),
        ]
    );
}

#[test]
fn says_are_bounded_and_blank_ones_stay_silent() {
    let mut reply = reply();
    let at_limit = "a".repeat(1_024);
    let out = run(&mut reply, |r, out| r.say(step(1, 0), &at_limit, out));
    assert_eq!(out.len(), 3, "queued, spoken and credited");
    assert_eq!(
        run(&mut reply, |r, out| r.say(
            step(1, 1),
            &"a".repeat(1_025),
            out
        )),
        []
    );
    assert_eq!(run(&mut reply, |r, out| r.say(step(1, 2), "  ", out)), []);
}

#[test]
fn past_256_segments_the_rest_stays_text() {
    let mut reply = reply();
    let out = run(&mut reply, |r, out| r.text(&"Go. ".repeat(257), out));
    let queued: Vec<_> = out
        .iter()
        .filter(|speech| matches!(speech, Speech::Queued { .. }))
        .collect();
    assert_eq!(queued.len(), 256);
    assert_eq!(queued[255], &self::queued(255));
}

#[test]
fn impossible_reports_are_refused() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 10], out));
    let mut out = Vec::new();
    assert_eq!(reply.progress(seg(9), 1, &mut out), Err(Refused::Unknown));
    assert_eq!(reply.progress(seg(0), 11, &mut out), Err(Refused::Total));
    assert_eq!(
        reply.progress(seg(1), 0, &mut out),
        Err(Refused::OutOfOrder)
    );
    assert_eq!(reply.played(seg(0), 10, &mut out), Err(Refused::OutOfOrder));
    run(&mut reply, |r, out| r.done(seg(0), out));
    assert_eq!(reply.played(seg(0), 9, &mut out), Err(Refused::Total));
    assert_eq!(reply.played(seg(0), 10, &mut out), Ok(()));
    assert_eq!(reply.played(seg(0), 10, &mut out), Err(Refused::OutOfOrder));
    assert_eq!(
        reply.progress(seg(0), 10, &mut out),
        Err(Refused::OutOfOrder)
    );
    assert_eq!(reply.stopped(None), Err(Refused::OutOfOrder));
}

#[test]
fn a_revoke_is_final_only_once_the_shell_is_quiet() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 100], out));
    run(&mut reply, |r, out| {
        r.progress(seg(0), 60, out).expect("plays");
    });
    run(&mut reply, super::Reply::close);
    let out = run(&mut reply, |r, out| r.revoke(Revoker::Brain, out));
    assert_eq!(out, [Speech::Revoke { tell_shell: true }]);
    assert_eq!(run(&mut reply, |r, out| r.revoke(Revoker::Shell, out)), []);
    assert_eq!(run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 9], out)), []);
    assert_eq!(run(&mut reply, |r, out| r.done(seg(0), out)), []);
    assert_eq!(
        run(&mut reply, |r, out| r.say(step(1, 0), "More.", out)),
        []
    );
    assert!(!reply.is_final());
    assert_eq!(
        reply.stopped(Some(Cursor {
            segment: seg(0),
            sample: 101,
        })),
        Err(Refused::Total)
    );
    assert!(!reply.is_final(), "a refused answer isn't quiet");
    assert_eq!(reply.stopped(None), Ok(()));
    assert!(reply.is_final());
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 60,
            text: 0,
        }),
        "no cursor keeps the point already heard"
    );
    assert_eq!(reply.stopped(None), Err(Refused::OutOfOrder));
}

#[test]
fn the_quiet_cursor_moves_the_heard_point() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 100], out));
    run(&mut reply, |r, out| r.done(seg(0), out));
    run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 100], out));
    run(&mut reply, |r, out| r.revoke(Revoker::Shell, out));
    assert_eq!(
        reply.stopped(Some(Cursor {
            segment: seg(1),
            sample: 30,
        })),
        Ok(())
    );
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(1),
            sample: 30,
            text: 2,
        })
    );
}

#[test]
fn a_device_change_stops_the_run_s_speech_and_its_text_stays() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. ", out));
    assert_eq!(
        run(&mut reply, |r, out| r.output(OutputGeneration(1), out)),
        []
    );
    let out = run(&mut reply, |r, out| r.output(OutputGeneration(2), out));
    assert_eq!(out, [Speech::Revoke { tell_shell: true }]);
    assert_eq!(
        run(&mut reply, |r, out| r.output(OutputGeneration(3), out)),
        []
    );
    assert_eq!(run(&mut reply, |r, out| r.text("B. C. ", out)), []);
    run(&mut reply, super::Reply::close);
    assert!(!reply.is_final());
    assert_eq!(reply.stopped(None), Ok(()));
    assert!(reply.is_final());
}

#[test]
fn the_stall_timer_counts_only_its_latest_arm_and_only_while_draining() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B", out));
    let mut out = Vec::new();
    reply.close(&mut out);
    let Some(Speech::Arm { epoch: first }) = out
        .iter()
        .rev()
        .find(|s| matches!(s, Speech::Arm { .. }))
        .cloned()
    else {
        panic!("closing arms the timer: {out:?}");
    };
    assert!(reply.stalled(first));
    let mut out = Vec::new();
    reply.pcm(seg(0), vec![5; 10], &mut out);
    let Some(Speech::Arm { epoch: second }) = out
        .iter()
        .find(|s| matches!(s, Speech::Arm { .. }))
        .cloned()
    else {
        panic!("audio re-arms it: {out:?}");
    };
    assert!(!reply.stalled(first));
    assert!(reply.stalled(second));
    let mut out = Vec::new();
    reply.progress(seg(0), 0, &mut out).expect("no progress");
    assert_eq!(out, [], "a report that doesn't advance doesn't re-arm");
    reply.revoke(Revoker::Brain, &mut Vec::new());
    assert!(
        !reply.stalled(second),
        "a revoked run waits on quiet, not the stall timer"
    );
}

#[test]
fn before_the_producer_closes_nothing_arms_the_timer() {
    let mut reply = reply();
    let mut out = Vec::new();
    reply.text("A. ", &mut out);
    reply.pcm(seg(0), vec![5; 10], &mut out);
    assert!(
        !out.iter()
            .any(|speech| matches!(speech, Speech::Arm { .. }))
    );
}

#[test]
fn heard_text_ends_at_the_last_played_text_segment() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("One. Two. ", out));
    speak_and_play(&mut reply, 0, 10);
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 10,
            text: 4,
        })
    );
    run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 10], out));
    run(&mut reply, |r, out| {
        r.progress(seg(1), 3, out).expect("plays");
    });
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(1),
            sample: 3,
            text: 4,
        })
    );
}

#[test]
fn a_failed_future_say_ends_only_after_the_segment_before_it() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Intro. ", out));
    run(&mut reply, |r, out| r.say(step(4, 0), "Diagram.", out));
    assert_eq!(run(&mut reply, |r, out| r.step_failed(SceneId(4), out)), []);
    assert_eq!(
        run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 10], out)),
        [pcm(0, 0, 10, None)]
    );
    assert_eq!(
        run(&mut reply, |r, out| r.done(seg(0), out)),
        [pcm(0, 1, 0, Some(10)), pcm(1, 0, 0, Some(0))]
    );
    assert_eq!(run(&mut reply, |r, out| r.done(seg(0), out)), []);
}

#[test]
fn a_scene_failure_after_a_revoke_sends_nothing() {
    for quiet in [false, true] {
        let mut reply = reply();
        run(&mut reply, |r, out| r.say(step(4, 0), "One.", out));
        run(&mut reply, |r, out| r.say(step(4, 1), "Two.", out));
        run(&mut reply, Reply::close);
        run(&mut reply, |r, out| r.revoke(Revoker::Brain, out));
        if quiet {
            reply.stopped(None).expect("quiet");
        }
        let mut out = Vec::new();
        reply.step_failed(SceneId(4), &mut out);
        assert_eq!(out, [], "quiet: {quiet}");
    }
}

#[test]
fn a_partly_failed_segment_is_heard_only_to_its_start() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("Hello. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 40], out));
    run(&mut reply, |r, out| r.failed(seg(0), Failure::Segment, out));
    run(&mut reply, |r, out| {
        r.played(seg(0), 40, out).expect("plays");
    });
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 40,
            text: 0,
        })
    );
}

#[test]
fn a_quiet_cursor_at_a_finished_segment_s_total_hears_its_text() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("One. Two. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 100], out));
    run(&mut reply, |r, out| r.done(seg(0), out));
    run(&mut reply, |r, out| r.revoke(Revoker::Shell, out));
    reply
        .stopped(Some(Cursor {
            segment: seg(0),
            sample: 100,
        }))
        .expect("quiet");
    assert_eq!(
        reply.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 100,
            text: 4,
        })
    );

    let mut open = self::reply();
    run(&mut open, |r, out| r.text("One. Two. ", out));
    run(&mut open, |r, out| r.pcm(seg(0), vec![5; 100], out));
    run(&mut open, |r, out| r.revoke(Revoker::Shell, out));
    open.stopped(Some(Cursor {
        segment: seg(0),
        sample: 100,
    }))
    .expect("quiet");
    assert_eq!(
        open.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 100,
            text: 0,
        }),
        "more audio could have followed"
    );
}

#[test]
fn a_quiet_cursor_can_t_skip_unfinished_speech_or_go_back() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. B. C. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 10], out));
    run(&mut reply, |r, out| r.done(seg(0), out));
    run(&mut reply, |r, out| r.pcm(seg(1), vec![5; 10], out));
    run(&mut reply, |r, out| r.revoke(Revoker::Shell, out));
    assert_eq!(
        reply.stopped(Some(Cursor {
            segment: seg(2),
            sample: 0,
        })),
        Err(Refused::OutOfOrder)
    );
    assert!(!reply.is_final());
    assert_eq!(reply.heard(), None);
    reply
        .stopped(Some(Cursor {
            segment: seg(1),
            sample: 5,
        }))
        .expect("reachable");
    assert!(reply.is_final());

    let mut back = self::reply();
    run(&mut back, |r, out| r.text("A. ", out));
    run(&mut back, |r, out| r.pcm(seg(0), vec![5; 10], out));
    run(&mut back, |r, out| {
        r.progress(seg(0), 8, out).expect("plays");
    });
    run(&mut back, |r, out| r.revoke(Revoker::Shell, out));
    assert_eq!(
        back.stopped(Some(Cursor {
            segment: seg(0),
            sample: 5,
        })),
        Err(Refused::OutOfOrder)
    );
    assert_eq!(
        back.heard(),
        Some(Heard {
            segment: seg(0),
            sample: 8,
            text: 0,
        })
    );

    let mut later = self::reply();
    run(&mut later, |r, out| r.text("A. B. ", out));
    speak_and_play(&mut later, 0, 10);
    run(&mut later, |r, out| r.pcm(seg(1), vec![5; 10], out));
    run(&mut later, |r, out| {
        r.progress(seg(1), 3, out).expect("plays");
    });
    run(&mut later, |r, out| r.revoke(Revoker::Shell, out));
    assert_eq!(
        later.stopped(Some(Cursor {
            segment: seg(0),
            sample: 10,
        })),
        Err(Refused::OutOfOrder),
        "segment 1 already played some"
    );
}

#[test]
fn after_quiet_late_reports_don_t_move_the_heard_point() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.text("A. ", out));
    run(&mut reply, |r, out| r.pcm(seg(0), vec![5; 10], out));
    run(&mut reply, |r, out| r.done(seg(0), out));
    run(&mut reply, |r, out| r.revoke(Revoker::Shell, out));
    reply
        .stopped(Some(Cursor {
            segment: seg(0),
            sample: 5,
        }))
        .expect("quiet");
    let heard = Some(Heard {
        segment: seg(0),
        sample: 5,
        text: 0,
    });
    assert_eq!(reply.progress(seg(0), 8, &mut Vec::new()), Ok(()));
    assert_eq!(reply.heard(), heard);
    assert_eq!(reply.played(seg(0), 10, &mut Vec::new()), Ok(()));
    assert_eq!(reply.heard(), heard);
}

#[test]
fn a_step_failure_that_changes_nothing_doesn_t_re_arm_the_timer() {
    let mut reply = reply();
    run(&mut reply, |r, out| r.say(step(4, 0), "One.", out));
    run(&mut reply, |r, out| r.text("Two. ", out));
    run(&mut reply, |r, out| r.step_failed(SceneId(4), out));
    let mut out = Vec::new();
    reply.close(&mut out);
    let Some(Speech::Arm { epoch }) = out.last().cloned() else {
        panic!("closing arms the timer: {out:?}");
    };
    for scene in [9, 4] {
        let mut out = Vec::new();
        reply.step_failed(SceneId(scene), &mut out);
        assert_eq!(out, [], "scene {scene}");
        assert!(reply.stalled(epoch), "scene {scene}");
    }
}

#[test]
fn debug_shows_no_text_or_audio() {
    let marker = "kumochi-reply-sentinel";
    let mut reply = reply();
    let mut out = Vec::new();
    reply.text(&format!("{marker}. {marker}"), &mut out);
    reply.pcm(seg(0), vec![12_345; 4], &mut out);
    let debug = format!("{reply:?} {out:?}");
    let bytes = format!("{:?}", marker.as_bytes());
    assert!(!debug.contains(marker), "{debug}");
    assert!(!debug.contains(&bytes[1..bytes.len() - 1]), "{debug}");
    assert!(
        !debug.contains("12345") && !debug.contains("57, 48"),
        "{debug}"
    );
}
