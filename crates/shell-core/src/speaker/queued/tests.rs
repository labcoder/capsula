use super::{Admission, CAPACITY, Credit, Fault, Packet, Queue, RATE};
use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId, SpeechPcm};

const RUN: RunId = RunId(17);
const LOGICAL: OutputGeneration = OutputGeneration(1);

fn frame(seq: u32, count: usize, last: Option<u32>, origin: u64) -> Packet {
    Packet {
        credit: Credit { origin, bytes: 50 },
        pcm: SpeechPcm {
            run: RUN,
            segment: SegmentId(0),
            output_generation: LOGICAL,
            seq,
            last,
            samples: vec![16_384; count],
        },
    }
}

#[test]
fn stop_fences_and_credits_held_ingress_without_consuming_it() {
    let mut queue = Queue::new(RUN, LOGICAL);
    let packet = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / packet).unwrap();
    for seq in 0..count {
        assert_eq!(
            queue.offer(frame(seq, packet, None, 1)),
            Ok(Admission::Return(Credit {
                origin: 1,
                bytes: 50
            }))
        );
    }
    assert_eq!(queue.samples.len(), CAPACITY);
    assert_eq!(queue.offer(frame(count, 1, None, 2)), Ok(Admission::Held));

    assert_eq!(
        queue.stop(),
        (
            CAPACITY,
            Some(Credit {
                origin: 2,
                bytes: 50
            })
        )
    );
    assert_eq!(queue.stop(), (0, None));
    assert_eq!(queue.retire(1), 0, "old presentation cannot free source");
    assert_eq!(
        queue.offer(frame(0, 1, None, 3)),
        Ok(Admission::Return(Credit {
            origin: 3,
            bytes: 50
        }))
    );
    assert_eq!(queue.samples.len(), 0);
}

#[test]
fn malformed_order_and_size_are_refused_even_when_full() {
    let mut queue = Queue::new(RUN, LOGICAL);
    let packet = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / packet).unwrap();
    for seq in 0..count {
        assert!(matches!(
            queue.offer(frame(seq, packet, None, 1)),
            Ok(Admission::Return(_))
        ));
    }
    assert_eq!(queue.offer(frame(count + 1, 1, None, 2)), Err(Fault::Order));
    assert_eq!(
        queue.offer(frame(count, packet + 1, None, 2)),
        Err(Fault::Size)
    );
    assert_eq!(queue.stop(), (CAPACITY, None));
}

#[test]
fn releasing_canonical_space_admits_the_held_frame_only_once() {
    let mut queue = Queue::new(RUN, LOGICAL);
    let packet = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / packet).unwrap();
    for seq in 0..count {
        assert!(matches!(
            queue.offer(frame(seq, packet, None, 1)),
            Ok(Admission::Return(_))
        ));
    }
    assert_eq!(queue.offer(frame(count, 1, None, 2)), Ok(Admission::Held));
    assert_eq!(queue.retry(), Ok(None));
    assert_eq!(queue.retire(1), 1);
    assert_eq!(
        queue.retry(),
        Ok(Some(Credit {
            origin: 2,
            bytes: 50
        }))
    );
    assert_eq!(queue.retry(), Ok(None));
    assert_eq!(queue.samples.len(), CAPACITY);
}

use super::timeline::{Anchor, Engine, Observation, Opened, Phase, Tail};
use std::time::Duration;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn segment(id: u32, count: usize) -> Packet {
    let mut packet = frame(0, count, Some(u32::try_from(count).unwrap()), 1);
    packet.pcm.segment = SegmentId(id);
    packet
}

fn presented(generation: u64, segment: u32, first: u64, through: u64, at: u64) -> Anchor {
    Anchor {
        run: RUN,
        generation,
        segment: SegmentId(segment),
        first,
        through,
        at: ms(at),
    }
}

#[test]
fn audible_virtual_audible_stop_cannot_bridge_the_heard_gap() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let a = engine.request(ms(0)).unwrap();
    assert!(engine.opened(a, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert_eq!(
        engine.observe(presented(a, 0, 0, 480, 20)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(20)), Ok(None));
    assert_eq!(engine.progress.heard, 480);

    assert!(engine.lost(a, 480, ms(20)));
    assert!(matches!(
        engine.offer(segment(1, 480), ms(20)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.tick(ms(40)), Ok(None));
    assert_eq!(engine.progress.consumed, 960);
    assert_eq!(engine.progress.heard, 480);
    assert_eq!(engine.progress.first_gap, Some(480));

    let c = engine.request(ms(1020)).unwrap();
    assert!(engine.opened(c, Opened::Physical, ms(1020)));
    assert!(matches!(
        engine.offer(segment(2, 480), ms(1020)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.offered, 960, "never replay B");
    engine.staged(1440);
    assert_eq!(
        engine.observe(presented(a, 2, 960, 1440, 1040)),
        Observation::Stale
    );
    assert_eq!(
        engine.observe(presented(c, 2, 960, 1440, 1040)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(1040)), Ok(None));
    assert_eq!(engine.stop(ms(1040)), (0, None));
    assert_eq!(
        engine.observe(presented(a, 1, 480, 960, 1041)),
        Observation::Stale
    );
    assert_eq!(engine.tick(ms(1060)), Ok(None));
    assert_eq!(engine.progress.consumed, 1440);
    assert_eq!(engine.progress.heard, 480);
    assert_eq!(engine.tail(), Tail::Unknown);
    assert!(!engine.final_at(ms(1060)));
    assert!(engine.acknowledge(c, Some(ms(1060))));
    assert!(!engine.final_at(ms(1060)));
    assert_eq!(engine.tail(), Tail::Unknown);
    assert!(engine.acknowledge(a, Some(ms(1050))));
    assert_eq!(engine.tail(), Tail::At(ms(1060)));
    assert!(!engine.final_at(ms(1059)));
    assert!(engine.final_at(ms(1060)));
    assert_eq!(engine.queue.completed, 3);
    assert!(engine.queue.segments.is_empty());
}

#[test]
fn virtual_clock_retires_full_queue_and_rebases_after_matching_resume() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let g = engine.request(ms(0)).unwrap();
    assert!(engine.opened(g, Opened::Unavailable, ms(0)));
    let packet = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / packet).unwrap();
    for seq in 0..count {
        assert!(matches!(
            engine.offer(frame(seq, packet, None, 1), ms(0)),
            Ok(Admission::Return(_))
        ));
    }
    assert_eq!(
        engine.offer(frame(count, 1, None, 2), ms(0)),
        Ok(Admission::Held)
    );
    engine.pause(40, ms(0));
    assert_eq!(engine.tick(ms(2000)), Ok(None));
    engine.resume(39, ms(2000));
    assert_eq!(engine.tick(ms(2200)), Ok(None));
    engine.resume(40, ms(2500));
    assert_eq!(
        engine.tick(ms(2501)),
        Ok(Some(Credit {
            origin: 2,
            bytes: 50
        }))
    );
    assert_eq!(engine.progress.consumed, 24);
    assert_eq!(engine.progress.heard, 0);
    assert_eq!(engine.tick(ms(2501)), Ok(None));
}

#[test]
fn rebuild_preserves_app_only_pcm_and_idle_uses_no_route_timer() {
    let mut engine = Engine::new(RUN, LOGICAL);
    assert_eq!(engine.request(ms(0)), None);
    engine.expect(ms(0));
    let g = engine.request(ms(0)).unwrap();
    assert!(engine.opened(g, Opened::Physical, ms(0)));
    assert_eq!(engine.phase, Phase::Buffering(1440));
    assert!(matches!(
        engine.offer(segment(0, 1920), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(1440);
    assert_eq!(
        engine.observe(presented(g, 0, 0, 480, 10)),
        Observation::Accepted
    );
    assert_eq!(
        engine.observe(presented(g, 0, 480, 720, 40)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(10)), Ok(None));
    assert!(engine.lost(g, 720, ms(20)));
    assert_eq!(engine.progress.heard, 480);
    assert_eq!(engine.progress.first_gap, Some(480));
    assert_eq!(engine.progress.consumed, 720);
    assert_eq!(engine.queue.samples.len(), 1200);
    assert_eq!(engine.offered, 720);
    assert_eq!(
        engine.observe(presented(g, 0, 720, 1440, 30)),
        Observation::Stale
    );
    assert_eq!(engine.tick(ms(80)), Ok(None));
    assert_eq!(engine.progress.consumed, 1920);
    assert_eq!(engine.tick(ms(1580)), Ok(None));
    assert_eq!(engine.phase, Phase::Idle);
    assert_eq!(engine.request(ms(8000)), None);
}

#[test]
fn a_pending_open_cannot_submit_audio_while_inhibited() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let request = engine.request(ms(0)).unwrap();
    engine.pause(12, ms(1));
    assert!(engine.opened(request, Opened::Physical, ms(2)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(2)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert_eq!(engine.offered, 0);
    engine.pause(11, ms(9));
    engine.resume(11, ms(10));
    engine.staged(480);
    assert_eq!(engine.offered, 0);
    engine.resume(12, ms(12));
    engine.pause(12, ms(13));
    engine.staged(480);
    assert_eq!(engine.offered, 480);
}

#[test]
fn stop_does_not_mark_discarded_pcm_as_completed() {
    let mut engine = Engine::new(RUN, LOGICAL);
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.stop(ms(0)), (480, None));
    assert_eq!(engine.queue.completed, 0);
    assert_eq!(engine.progress.consumed, 0);
    assert_eq!(engine.progress.heard, 0);
}

fn observation_engine() -> (Engine, u64) {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let generation = engine.request(ms(0)).unwrap();
    assert!(engine.opened(generation, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    (engine, generation)
}

#[test]
fn contiguous_observations_advance_device_presentation_after_their_deadline() {
    let (mut engine, generation) = observation_engine();
    for end in 1..=16 {
        assert_eq!(
            engine.observe(presented(generation, 0, end - 1, end, end)),
            Observation::Accepted
        );
    }
    assert_eq!(engine.tick(ms(16)), Ok(None));
    assert_eq!(engine.progress.heard, 16);
    assert_eq!(
        engine.observe(presented(generation, 0, 16, 17, 17)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(17)), Ok(None));
    assert_eq!(engine.progress.heard, 17);
}

#[test]
fn a_gap_or_backwards_time_requires_recovery() {
    let (mut gap, generation) = observation_engine();
    assert_eq!(
        gap.observe(presented(generation, 0, 1, 480, 10)),
        Observation::Recover
    );
    assert_eq!(
        gap.observe(presented(generation, 0, 0, 16, 10)),
        Observation::Recover
    );

    let (mut backwards, generation) = observation_engine();
    assert_eq!(
        backwards.observe(presented(generation, 0, 0, 16, 16)),
        Observation::Accepted
    );
    assert_eq!(backwards.tick(ms(16)), Ok(None));
    assert_eq!(
        backwards.observe(presented(generation, 0, 16, 17, 15)),
        Observation::Recover
    );
}

#[test]
fn a_failed_open_does_not_rebase_the_running_virtual_clock() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let generation = engine.request(ms(0)).unwrap();
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.tick(ms(10)), Ok(None));
    assert!(engine.opened(generation, Opened::Unavailable, ms(15)));
    assert_eq!(engine.tick(ms(20)), Ok(None));
    assert_eq!(engine.progress.consumed, 480);
    assert_eq!(engine.progress.heard, 0);
}

fn losing_a() -> (Engine, u64) {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let a = engine.request(ms(0)).unwrap();
    assert!(engine.opened(a, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert_eq!(
        engine.observe(presented(a, 0, 0, 480, 20)),
        Observation::Accepted
    );
    assert!(engine.lost(a, 480, ms(10)));
    (engine, a)
}

fn stop_b(engine: &mut Engine) -> u64 {
    let b = engine.request(ms(1000)).unwrap();
    assert!(engine.opened(b, Opened::Physical, ms(1000)));
    assert!(matches!(
        engine.offer(segment(1, 480), ms(1000)),
        Ok(Admission::Return(_))
    ));
    engine.staged(960);
    assert_eq!(
        engine.observe(presented(b, 1, 480, 960, 1020)),
        Observation::Accepted
    );
    engine.stop(ms(1020));
    assert!(engine.acknowledge(b, Some(ms(1040))));
    b
}

#[test]
fn review_b_cannot_settle_a_unknown_tail() {
    let (mut engine, a) = losing_a();
    assert!(engine.acknowledge(a, None));
    let b = stop_b(&mut engine);
    assert!(!engine.final_at(ms(1040)));
    assert!(engine.acknowledge(a, Some(ms(1060))));
    assert!(!engine.final_at(ms(1059)));
    assert!(engine.final_at(ms(1060)));
    assert!(!engine.lost(b, 960, ms(1061)));
    assert!(!engine.acknowledge(b, Some(ms(9999))));
    assert_eq!(
        engine.observe(presented(b, 1, 480, 960, 1061)),
        Observation::Stale
    );
    assert!(engine.final_at(ms(1061)));
}

#[test]
fn review_b_cannot_shorten_a_later_known_tail() {
    let (mut engine, a) = losing_a();
    assert!(engine.acknowledge(a, Some(ms(1080))));
    stop_b(&mut engine);
    assert!(!engine.final_at(ms(1079)));
    assert!(engine.final_at(ms(1080)));
}

#[test]
fn review_successful_open_settles_virtual_time_without_a_tick() {
    for tick_first in [false, true] {
        let mut engine = Engine::new(RUN, LOGICAL);
        engine.expect(ms(0));
        let g = engine.request(ms(0)).unwrap();
        assert!(matches!(
            engine.offer(segment(0, 480), ms(0)),
            Ok(Admission::Return(_))
        ));
        if tick_first {
            assert_eq!(engine.tick(ms(20)), Ok(None));
        }
        assert!(engine.opened(g, Opened::Physical, ms(20)));
        assert_eq!(engine.progress.consumed, 480);
        assert_eq!(engine.progress.heard, 0);
        assert_eq!(engine.offered, 480);
        assert!(engine.queue.samples.is_empty());
    }
}

#[test]
fn review_pause_settles_elapsed_time_and_keeps_the_fraction() {
    for tick_first in [false, true] {
        let mut engine = Engine::new(RUN, LOGICAL);
        assert!(matches!(
            engine.offer(segment(0, 480), ms(0)),
            Ok(Admission::Return(_))
        ));
        let before = Duration::from_micros(10_030);
        if tick_first {
            assert_eq!(engine.tick(before), Ok(None));
        }
        engine.pause(1, before);
        assert_eq!(engine.progress.consumed, 240);
        engine.resume(1, ms(100));
        assert_eq!(engine.tick(Duration::from_micros(100_012)), Ok(None));
        assert_eq!(engine.progress.consumed, 241);
    }
}

#[test]
fn review_new_pcm_cannot_spend_time_from_before_admission() {
    let mut engine = Engine::new(RUN, LOGICAL);
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert!(matches!(
        engine.offer(segment(1, 480), ms(40)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.tick(ms(40)), Ok(None));
    assert_eq!(engine.progress.consumed, 480);
    assert_eq!(engine.queue.samples.len(), 480);
}

#[test]
fn review_stop_applies_due_but_not_future_presentation_without_a_tick() {
    for tick_first in [false, true] {
        let mut engine = Engine::new(RUN, LOGICAL);
        engine.expect(ms(0));
        let g = engine.request(ms(0)).unwrap();
        assert!(engine.opened(g, Opened::Physical, ms(0)));
        assert!(matches!(
            engine.offer(segment(0, 960), ms(0)),
            Ok(Admission::Return(_))
        ));
        engine.staged(960);
        assert_eq!(
            engine.observe(presented(g, 0, 0, 480, 20)),
            Observation::Accepted
        );
        assert_eq!(
            engine.observe(presented(g, 0, 480, 960, 22)),
            Observation::Accepted
        );
        if tick_first {
            assert_eq!(engine.tick(ms(21)), Ok(None));
        }
        assert_eq!(engine.stop(ms(21)), (480, None));
        assert_eq!(engine.progress.consumed, 480);
        assert_eq!(engine.progress.heard, 480);
        assert_eq!(engine.queue.completed, 0);
    }
}

#[test]
fn review_stale_logical_generation_returns_credit_without_taking_sequence() {
    let mut queue = Queue::new(RUN, LOGICAL);
    let mut stale = frame(0, 1, None, 2);
    stale.pcm.output_generation = OutputGeneration(2);
    assert_eq!(
        queue.offer(stale),
        Ok(Admission::Return(Credit {
            origin: 2,
            bytes: 50
        }))
    );
    assert!(queue.samples.is_empty());
    assert_eq!(
        queue.offer(frame(0, 1, None, 1)),
        Ok(Admission::Return(Credit {
            origin: 1,
            bytes: 50
        }))
    );
    assert_eq!(queue.samples.len(), 1);
    queue.stop();
    let mut successor = frame(0, 1, None, 3);
    successor.pcm.run = RunId(RUN.0 + 1);
    assert_eq!(queue.offer(successor), Err(Fault::Order));
}

#[test]
fn review_delayed_loss_does_not_confirm_an_estimate_that_was_pending() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let g = engine.request(ms(0)).unwrap();
    assert!(engine.opened(g, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 960), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(960);
    assert_eq!(
        engine.observe(presented(g, 0, 0, 240, 10)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(10)), Ok(None));
    assert_eq!(
        engine.observe(presented(g, 0, 240, 480, 20)),
        Observation::Accepted
    );
    assert!(engine.lost(g, 480, ms(30)));
    assert_eq!(engine.progress.heard, 240);
    assert_eq!(engine.progress.first_gap, Some(240));
    assert_eq!(engine.progress.consumed, 480);
    assert_eq!(engine.queue.samples.len(), 480);
}

#[test]
fn review_observation_overflow_recovers_through_virtual_without_laundering_the_gap() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let g = engine.request(ms(0)).unwrap();
    assert!(engine.opened(g, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert_eq!(
        engine.observe(presented(g, 0, 0, 24, 1)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(1)), Ok(None));
    for end in 25..=40 {
        assert_eq!(
            engine.observe(presented(g, 0, end - 1, end, 20)),
            Observation::Accepted
        );
    }
    assert_eq!(
        engine.observe(presented(g, 0, 40, 41, 20)),
        Observation::Recover
    );
    assert!(engine.lost(g, 41, ms(30)));
    assert_eq!(engine.progress.consumed, 41);
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.progress.first_gap, Some(24));
    assert_eq!(engine.tick(ms(50)), Ok(None));
    assert_eq!(engine.progress.consumed, 480);
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.queue.completed, 1);
}

#[test]
fn review_virtual_open_at_full_queue_returns_held_credit_once() {
    for tick_first in [false, true] {
        let mut engine = Engine::new(RUN, LOGICAL);
        engine.expect(ms(0));
        let g = engine.request(ms(0)).unwrap();
        let packet = usize::try_from(RATE / 10).unwrap();
        let count = u32::try_from(CAPACITY / packet).unwrap();
        for seq in 0..count {
            assert!(matches!(
                engine.offer(frame(seq, packet, None, 1), ms(0)),
                Ok(Admission::Return(_))
            ));
        }
        assert_eq!(
            engine.offer(frame(count, 1, None, 2), ms(0)),
            Ok(Admission::Held)
        );
        let expected = Ok(Some(Credit {
            origin: 2,
            bytes: 50,
        }));
        if tick_first {
            assert_eq!(engine.tick(ms(1)), expected);
        }
        assert!(engine.opened(g, Opened::Physical, ms(1)));
        assert_eq!(engine.progress.consumed, 24);
        assert_eq!(
            engine.tick(ms(1)),
            if tick_first { Ok(None) } else { expected }
        );
        assert_eq!(engine.tick(ms(1)), Ok(None));
    }
}

#[test]
fn review_prime_and_refill_targets_have_exact_open_segment_boundaries() {
    let mut engine = Engine::new(RUN, LOGICAL);
    assert!(matches!(
        engine.offer(frame(0, 1439, None, 1), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.phase, Phase::Buffering(1440));
    assert!(matches!(
        engine.offer(frame(1, 1, None, 1), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.phase, Phase::Running);
    assert_eq!(engine.tick(ms(60)), Ok(None));
    assert_eq!(engine.phase, Phase::Buffering(960));
    assert!(matches!(
        engine.offer(frame(2, 959, None, 1), ms(60)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.phase, Phase::Buffering(960));
    assert!(matches!(
        engine.offer(frame(3, 1, None, 1), ms(60)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.phase, Phase::Running);
}

#[test]
fn an_unresolved_predecessor_and_replacement_bound_further_opens() {
    let (mut engine, a) = losing_a();
    let b = engine.request(ms(1000)).unwrap();
    assert!(engine.opened(b, Opened::Physical, ms(1000)));
    assert!(matches!(
        engine.offer(segment(1, 480), ms(1000)),
        Ok(Admission::Return(_))
    ));
    engine.staged(960);
    assert_eq!(
        engine.observe(presented(b, 1, 480, 960, 1040)),
        Observation::Accepted
    );
    assert!(engine.lost(b, 960, ms(1020)));
    assert!(matches!(
        engine.offer(segment(2, 480), ms(1020)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.tick(ms(1040)), Ok(None));
    assert_eq!(engine.progress.consumed, 1440);
    assert_eq!(engine.request(ms(2000)), None);
    assert!(engine.acknowledge(a, Some(ms(1080))));
    assert!(engine.request(ms(2000)).is_some());
    assert_eq!(engine.tail(), Tail::Unknown);
}

#[test]
fn stale_ingress_does_not_itself_advance_virtual_playback() {
    let mut engine = Engine::new(RUN, LOGICAL);
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    let mut stale = segment(1, 480);
    stale.pcm.output_generation = OutputGeneration(2);
    assert!(matches!(
        engine.offer(stale, ms(20)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.progress.consumed, 0);
    assert_eq!(engine.queue.samples.len(), 480);
    assert_eq!(engine.tick(ms(20)), Ok(None));
    assert_eq!(engine.progress.consumed, 480);
}

#[test]
fn retired_output_is_stale_before_successor_order() {
    let current = OutputGeneration(2);
    let mut engine = Engine::new(RUN, current);
    let packet = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / packet).unwrap();
    for seq in 0..count {
        let mut current_frame = frame(seq, packet, None, 1);
        current_frame.pcm.output_generation = current;
        assert!(matches!(
            engine.offer(current_frame, ms(0)),
            Ok(Admission::Return(_))
        ));
    }
    let mut held = frame(count, 1, None, 2);
    held.pcm.output_generation = current;
    assert_eq!(engine.offer(held, ms(0)), Ok(Admission::Held));

    let mut stale = frame(0, 1, None, 3);
    stale.pcm.run = RunId(18);
    assert_eq!(
        engine.offer(stale, ms(1)),
        Ok(Admission::Return(Credit {
            origin: 3,
            bytes: 50
        }))
    );
    assert_eq!(engine.queue.samples.len(), CAPACITY);
    assert_eq!(engine.progress.consumed, 0);
    assert_eq!(engine.queue.held.as_ref().unwrap().pcm.seq, count);
    assert_eq!(
        engine.tick(ms(1)),
        Ok(Some(Credit {
            origin: 2,
            bytes: 50
        }))
    );
    assert_eq!(engine.queue.segments.back().unwrap().next_seq, count + 1);
    assert_eq!(engine.tick(ms(1)), Ok(None));
}

fn recovery_with_held_ingress() -> (Engine, u64) {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let generation = engine.request(ms(0)).unwrap();
    assert!(engine.opened(generation, Opened::Physical, ms(0)));
    let size = usize::try_from(RATE / 10).unwrap();
    let count = u32::try_from(CAPACITY / size).unwrap();
    for seq in 0..count {
        assert!(matches!(
            engine.offer(frame(seq, size, None, 1), ms(0)),
            Ok(Admission::Return(_))
        ));
    }
    engine.staged(480);
    assert_eq!(
        engine.observe(presented(generation, 0, 0, 24, 1)),
        Observation::Accepted
    );
    assert_eq!(engine.tick(ms(1)), Ok(None));
    assert_eq!(
        engine.offer(frame(count, 25, None, 2), ms(1)),
        Ok(Admission::Held)
    );
    for end in 25..=40 {
        assert_eq!(
            engine.observe(presented(generation, 0, end - 1, end, 20)),
            Observation::Accepted
        );
    }
    assert_eq!(
        engine.observe(presented(generation, 0, 40, 41, 20)),
        Observation::Recover
    );
    (engine, generation)
}

#[test]
fn recovery_before_stop_fences_pending_presentation_and_returns_held_credit() {
    let (mut engine, generation) = recovery_with_held_ingress();
    let (discarded, credit) = engine.stop(ms(30));
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.progress.consumed, 24);
    assert_eq!(discarded, CAPACITY - 24);
    assert_eq!(
        credit,
        Some(Credit {
            origin: 2,
            bytes: 50
        })
    );
    assert!(!engine.lost(generation, 41, ms(31)));
    assert_eq!(engine.stop(ms(32)), (0, None));
    assert_eq!(engine.tail(), Tail::Unknown);
}

#[test]
fn recovery_before_pause_keeps_credit_held_until_actual_handoff_is_retired() {
    let (mut engine, generation) = recovery_with_held_ingress();
    engine.pause(9, ms(30));
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.progress.consumed, 24);
    assert_eq!(engine.tick(ms(30)), Ok(None));
    assert_eq!(
        engine.queue.held.as_ref().unwrap().credit,
        Credit {
            origin: 2,
            bytes: 50
        }
    );
    engine.resume(9, ms(31));
    engine.staged(481);
    assert_eq!(engine.offered, 480);
    assert!(engine.lost(generation, 41, ms(31)));
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.progress.consumed, 41);
    assert_eq!(engine.progress.first_gap, Some(24));
    assert_eq!(
        engine.tick(ms(31)),
        Ok(Some(Credit {
            origin: 2,
            bytes: 50
        }))
    );
    assert_eq!(engine.tick(ms(32)), Ok(None));
    assert_eq!(engine.progress.consumed, 65);
    assert_eq!(engine.progress.heard, 24);
    assert_eq!(engine.progress.first_gap, Some(24));
}

#[test]
fn a_stale_open_result_cannot_advance_the_virtual_clock() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let request = engine.request(ms(0)).unwrap();
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert!(!engine.opened(request + 1, Opened::Physical, ms(10)));
    assert_eq!(engine.progress.consumed, 0);
    assert_eq!(engine.tick(ms(10)), Ok(None));
    assert_eq!(engine.progress.consumed, 240);
}

#[test]
fn feeding_copies_only_unstaged_source_through_one_segment() {
    let mut engine = Engine::new(RUN, LOGICAL);
    engine.expect(ms(0));
    let generation = engine.request(ms(0)).unwrap();
    assert!(engine.opened(generation, Opened::Physical, ms(0)));
    let mut first = segment(0, 481);
    first.pcm.samples[480] = 23;
    assert_eq!(
        engine.offer(first, ms(0)),
        Ok(Admission::Return(Credit {
            origin: 1,
            bytes: 50
        }))
    );
    let mut second = segment(1, 3);
    second.pcm.samples.fill(42);
    assert!(matches!(
        engine.offer(second, ms(0)),
        Ok(Admission::Return(_))
    ));
    let mut out = [0; 480];
    let offer = engine.feed(&mut out, None).unwrap();
    assert_eq!(
        (offer.segment, offer.at, offer.len, offer.last),
        (SegmentId(0), 0, 480, false)
    );
    assert_eq!(out, [16_384; 480]);
    assert_eq!(engine.progress.consumed, 0);
    engine.staged(480);
    let offer = engine.feed(&mut out, None).unwrap();
    assert_eq!(
        (offer.segment, offer.at, offer.len, offer.last),
        (SegmentId(0), 480, 1, true)
    );
    assert_eq!(out[0], 23);
    engine.staged(481);
    let offer = engine.feed(&mut out, Some(SegmentId(0))).unwrap();
    assert_eq!(
        (offer.segment, offer.at, offer.len, offer.last),
        (SegmentId(1), 481, 3, true)
    );
    assert_eq!(&out[..3], &[42; 3]);
    engine.staged(484);
    assert!(matches!(
        engine.offer(segment(2, 0), ms(0)),
        Ok(Admission::Return(_))
    ));
    let offer = engine.feed(&mut out, Some(SegmentId(1))).unwrap();
    assert_eq!(
        (offer.segment, offer.at, offer.len, offer.last),
        (SegmentId(2), 484, 0, true)
    );
    assert!(engine.feed(&mut out, Some(SegmentId(2))).is_none());
    assert_eq!(engine.progress.consumed, 0);
}

#[test]
fn held_or_recovering_routes_cannot_offer_more_source() {
    let (mut engine, generation) = observation_engine();
    let mut out = [0; 480];
    engine.pause(1, ms(0));
    assert!(engine.feed(&mut out, None).is_none());
    engine.resume(1, ms(0));
    assert_eq!(engine.feed(&mut out, None).unwrap().len, 0);
    assert_eq!(
        engine.observe(presented(generation, 0, 1, 16, 10)),
        Observation::Recover
    );
    assert!(engine.feed(&mut out, None).is_none());
}
