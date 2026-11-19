use super::{Admission, CAPACITY, Credit, Fault, Packet, Queue, RATE};
use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId, SpeechPcm};

const RUN: RunId = RunId(17);

fn frame(seq: u32, count: usize, last: Option<u32>, origin: u64) -> Packet {
    Packet {
        credit: Credit { origin, bytes: 50 },
        pcm: SpeechPcm {
            run: RUN,
            segment: SegmentId(0),
            output_generation: OutputGeneration(1),
            seq,
            last,
            samples: vec![16_384; count],
        },
    }
}

#[test]
fn stop_fences_and_credits_held_ingress_without_consuming_it() {
    let mut queue = Queue::new(RUN);
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
    let mut queue = Queue::new(RUN);
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
    let mut queue = Queue::new(RUN);
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

use super::timeline::{Anchor, Engine, Opened, Phase, Tail};
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
    let mut engine = Engine::new(RUN);
    engine.expect(ms(0));
    let a = engine.request(ms(0)).unwrap();
    assert!(engine.opened(a, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert!(engine.observe(presented(a, 0, 0, 480, 20)));
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
    assert!(engine.observe(presented(c, 2, 960, 1440, 1040)));
    assert_eq!(engine.tick(ms(1040)), Ok(None));
    assert_eq!(engine.stop(ms(1040)), (0, None));
    assert!(!engine.observe(presented(a, 1, 480, 960, 1041)));
    assert_eq!(engine.tick(ms(1060)), Ok(None));
    assert_eq!(engine.progress.consumed, 1440);
    assert_eq!(engine.progress.heard, 480);
    assert_eq!(engine.tail, Tail::Unknown);
    assert!(!engine.final_at(ms(1060)));
    assert!(engine.acknowledge(c, Some(ms(1060))));
    assert_eq!(engine.tail, Tail::At(ms(1060)));
    assert!(!engine.final_at(ms(1059)));
    assert!(engine.final_at(ms(1060)));
    assert_eq!(engine.queue.completed, 3);
    assert!(engine.queue.segments.is_empty());
}

#[test]
fn virtual_clock_retires_full_queue_and_rebases_after_matching_resume() {
    let mut engine = Engine::new(RUN);
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
    let mut engine = Engine::new(RUN);
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
    assert!(engine.observe(presented(g, 0, 0, 480, 10)));
    assert!(engine.observe(presented(g, 0, 480, 720, 40)));
    assert!(engine.lost(g, 720, ms(20)));
    assert_eq!(engine.progress.heard, 480);
    assert_eq!(engine.progress.first_gap, Some(480));
    assert_eq!(engine.progress.consumed, 720);
    assert_eq!(engine.queue.samples.len(), 1200);
    assert_eq!(engine.offered, 720);
    assert!(!engine.observe(presented(g, 0, 720, 1440, 30)));
    assert_eq!(engine.tick(ms(80)), Ok(None));
    assert_eq!(engine.progress.consumed, 1920);
    assert_eq!(engine.tick(ms(1580)), Ok(None));
    assert_eq!(engine.phase, Phase::Idle);
    assert_eq!(engine.request(ms(8000)), None);
}

#[test]
fn a_pending_open_cannot_submit_audio_while_inhibited() {
    let mut engine = Engine::new(RUN);
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
    let mut engine = Engine::new(RUN);
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    assert_eq!(engine.stop(ms(0)), (480, None));
    assert_eq!(engine.queue.completed, 0);
    assert_eq!(engine.progress.consumed, 0);
    assert_eq!(engine.progress.heard, 0);
}

#[test]
fn only_contiguous_observations_can_advance_device_presentation() {
    let mut engine = Engine::new(RUN);
    engine.expect(ms(0));
    let generation = engine.request(ms(0)).unwrap();
    assert!(engine.opened(generation, Opened::Physical, ms(0)));
    assert!(matches!(
        engine.offer(segment(0, 480), ms(0)),
        Ok(Admission::Return(_))
    ));
    engine.staged(480);
    assert!(!engine.observe(presented(generation, 0, 1, 480, 10)));
    for end in 1..=16 {
        assert!(engine.observe(presented(generation, 0, end - 1, end, end)));
    }
    assert!(!engine.observe(presented(generation, 0, 16, 17, 17)));
    assert_eq!(engine.tick(ms(16)), Ok(None));
    assert_eq!(engine.progress.heard, 16);
    assert!(!engine.observe(presented(generation, 0, 16, 17, 15)));
    assert!(engine.observe(presented(generation, 0, 16, 17, 17)));
    assert_eq!(engine.tick(ms(17)), Ok(None));
    assert_eq!(engine.progress.heard, 17);
}

#[test]
fn a_failed_open_does_not_rebase_the_running_virtual_clock() {
    let mut engine = Engine::new(RUN);
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
