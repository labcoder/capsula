use super::*;
use crate::platform::speaker_cpal::tests::FakePlayback;
use kumochi_protocol::message::SpeechPcm;

const RUN: RunId = RunId(17);
const OUTPUT: OutputGeneration = OutputGeneration(2);
const INSTANT: Duration = Duration::ZERO;

fn fake() -> (Worker, Input, Port, Receiver<Request>, SyncSender<Reply>) {
    let (send, requests) = mpsc::sync_channel(1);
    let (reply, replies) = mpsc::sync_channel(1);
    let (commands, pending) = mpsc::sync_channel(COMMANDS);
    let (packets, data) = mpsc::sync_channel(1);
    let busy = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    (
        Worker {
            engine: Engine::new(RUN, OUTPUT),
            physical: None,
            job: None,
            pending_packet: None,
            send,
            replies,
            route_closed: false,
        },
        Input {
            worker: thread::current(),
            commands,
            packets,
            busy: Arc::clone(&busy),
            stop: Arc::clone(&stop),
            closed: Arc::clone(&closed),
        },
        Port {
            commands: pending,
            packets: data,
            busy,
            stop,
            closed,
        },
        requests,
        reply,
    )
}

fn packet(origin: u64, seq: u32, samples: usize, last: Option<u32>) -> Packet {
    Packet {
        credit: Credit { origin, bytes: 50 },
        pcm: SpeechPcm {
            run: RUN,
            output_generation: OUTPUT,
            segment: SegmentId(0),
            seq,
            last,
            samples: vec![16_384; samples],
        },
    }
}

#[test]
fn stop_is_processed_while_the_open_and_ingress_mailboxes_are_busy() {
    let (worker, mut input, port, requests, reply) = fake();
    let (credits, received) = mpsc::sync_channel(1);
    let (closed, done) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        worker.serve(&port, Instant::now(), |credit| {
            credits.send(credit).unwrap();
        });
        closed.send(()).unwrap();
    });
    input.worker = owner.thread().clone();
    input.command(Command::Expect).unwrap();
    let Request::Open(generation, control) = requests.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("expected a pending open");
    };

    input.command(Command::Stop).unwrap();
    input.submit(packet(7, 0, 1, Some(1))).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(2)),
        Ok(Credit {
            origin: 7,
            bytes: 50
        })
    );
    assert_eq!(control.fenced_handoff(), Some(0));
    assert!(requests.try_recv().is_err());

    drop(input);
    let detached = done.recv_timeout(Duration::from_secs(2));
    if detached.is_err() {
        reply.send(Reply::Opened(generation, None)).unwrap();
        owner.thread().unpark();
    }
    owner.join().unwrap();
    assert_eq!(detached, Ok(()));
}

#[test]
fn a_late_physical_open_is_retired_without_feeding_or_waiting_for_backend_drop() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut credits = Vec::new();
    let mut grant = |credit| credits.push(credit);

    input.command(Command::Expect).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    assert!(matches!(
        input.submit(packet(9, 1, 1, Some(481))),
        Err(TrySendError::Full(_))
    ));
    input.command(Command::Stop).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();

    let (transport, mut playback) = FakePlayback::open(control, &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let mut out = [1.0; 16];
    playback.fill(&mut out, INSTANT);
    assert_eq!(out.map(f32::to_bits), [0; 16]);
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Retire(retired) = requests.try_recv().unwrap() else {
        panic!("expected retirement")
    };
    assert_eq!(retired, generation);

    input.submit(packet(8, 0, 1, Some(1))).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    assert_eq!(worker.engine.progress.heard, 0);
    assert_eq!(worker.engine.progress.consumed, 0);
    assert_eq!(
        credits,
        [
            Credit {
                origin: 7,
                bytes: 50
            },
            Credit {
                origin: 8,
                bytes: 50
            }
        ]
    );
}

#[test]
fn overflow_during_playback_retires_actual_handoff_without_promoting_pending_estimates() {
    const EARLY: Duration = Duration::from_millis(2);
    const PREDICTED: Duration = Duration::from_millis(20);
    const LOSS: Duration = Duration::from_millis(30);
    const RAMP: Duration = Duration::from_millis(31);
    static OCCUPIED: AtomicBool = AtomicBool::new(false);

    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut credits = Vec::new();
    let mut grant = |credit| credits.push(credit);

    input.command(Command::Expect).unwrap();
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    let (transport, mut playback) = FakePlayback::open(Arc::clone(&control), &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    playback.fill(&mut [0.0; 24], INSTANT);
    worker.step(&port, &mut pcm, &mut grant, EARLY).unwrap();
    assert_eq!(worker.engine.progress.heard, 24);

    for _ in 0..9 {
        playback.fill(&mut [0.0; 2], PREDICTED);
    }
    assert!(control.overflowed());
    worker.step(&port, &mut pcm, &mut grant, LOSS).unwrap();
    assert_eq!(worker.engine.progress.consumed, 42);
    assert_eq!(worker.engine.progress.heard, 24);
    assert_eq!(worker.engine.progress.first_gap, Some(24));

    input.command(Command::Stop).unwrap();
    worker.step(&port, &mut pcm, &mut grant, LOSS).unwrap();
    assert!(!worker.engine.final_at(LOSS));
    let mut out = [99.0; 125];
    playback.fill(&mut out, RAMP);
    assert!(out[119..].iter().all(|sample| sample.to_bits() == 0));
    worker.step(&port, &mut pcm, &mut grant, RAMP).unwrap();
    assert!(!worker.engine.final_at(RAMP));
    assert!(worker.engine.final_at(Duration::from_millis(36)));
    assert!(matches!(requests.try_recv(), Ok(Request::Retire(id)) if id == generation));
    assert_eq!(worker.engine.progress.heard, 24);
    assert_eq!(
        credits,
        [Credit {
            origin: 7,
            bytes: 50
        }]
    );
}

#[test]
fn a_failed_terminal_wake_retires_with_an_unknown_tail_without_retrying_power() {
    const IDLE: Duration = Duration::from_millis(1_500);
    static OCCUPIED: AtomicBool = AtomicBool::new(false);

    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut grant = |_| panic!("there is no ingress to credit");
    input.command(Command::Expect).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    let (transport, _playback) = FakePlayback::open(control, &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();

    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();
    assert!(
        matches!(requests.try_recv(), Ok(Request::Power(id, Power::Paused)) if id == generation)
    );
    assert!(reply.send(Reply::Powered(generation, true)).is_ok());
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();
    input.command(Command::Stop).unwrap();
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();
    assert!(
        matches!(requests.try_recv(), Ok(Request::Power(id, Power::Playing)) if id == generation)
    );
    assert!(reply.send(Reply::Powered(generation, false)).is_ok());
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();

    assert!(matches!(requests.try_recv(), Ok(Request::Retire(id)) if id == generation));
    assert!(!worker.engine.final_at(IDLE));
}

#[test]
fn stop_fences_once_even_when_the_command_mailbox_is_full() {
    let (mut worker, input, port, requests, _reply) = fake();
    let mut pcm = [0; 480];
    let mut credits = Vec::new();
    let mut grant = |credit| credits.push(credit);

    for _ in 0..COMMANDS {
        input.command(Command::Expect).unwrap();
    }
    assert!(matches!(
        input.command(Command::Expect),
        Err(TrySendError::Full(_))
    ));
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    assert!(input.command(Command::Stop).is_ok());
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    assert!(worker.engine.queue.stopped);
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    assert_eq!(worker.engine.progress.consumed, 0);
    assert_eq!(
        credits,
        [Credit {
            origin: 7,
            bytes: 50
        }]
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn a_hung_resume_moves_to_virtual_and_releases_held_credit() {
    const IDLE: Duration = Duration::from_millis(1_500);
    static OCCUPIED: AtomicBool = AtomicBool::new(false);

    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut credits = Vec::new();
    let mut grant = |credit| credits.push(credit);

    input.command(Command::Expect).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    let (transport, mut playback) = FakePlayback::open(Arc::clone(&control), &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();
    assert!(
        matches!(requests.try_recv(), Ok(Request::Power(id, Power::Paused)) if id == generation)
    );
    assert!(reply.send(Reply::Powered(generation, true)).is_ok());
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();

    let samples = 2_400;
    let frames = u32::try_from(super::super::CAPACITY / samples).unwrap();
    for seq in 0..frames {
        assert_eq!(
            worker.engine.offer(packet(1, seq, samples, None), IDLE),
            Ok(Admission::Return(Credit {
                origin: 1,
                bytes: 50
            }))
        );
    }
    input.submit(packet(7, frames, 1, None)).unwrap();
    worker.step(&port, &mut pcm, &mut grant, IDLE).unwrap();
    assert!(
        matches!(requests.try_recv(), Ok(Request::Power(id, Power::Playing)) if id == generation)
    );
    assert!(worker.engine.queue.held.is_some());

    let expired = IDLE + STALL;
    assert_eq!(worker.deadline(IDLE), Some(expired));
    let before = expired.checked_sub(Duration::from_millis(1)).unwrap();
    playback.fill(&mut [], before);
    worker.step(&port, &mut pcm, &mut grant, before).unwrap();
    assert_eq!(worker.deadline(before), Some(expired));
    assert_eq!(control.fenced_handoff(), None);
    worker.step(&port, &mut pcm, &mut grant, expired).unwrap();
    assert_eq!(control.fenced_handoff(), Some(0));
    let later = expired + Duration::from_millis(1);
    worker.step(&port, &mut pcm, &mut grant, later).unwrap();
    assert_eq!(worker.engine.progress.consumed, 24);
    assert_eq!(worker.engine.progress.heard, 0);
    assert!(worker.engine.queue.held.is_none());
    assert!(requests.try_recv().is_err());

    let mut out = [99.0; 4];
    assert!(reply.send(Reply::Powered(generation, true)).is_ok());
    worker.step(&port, &mut pcm, &mut grant, later).unwrap();
    playback.fill(&mut out, later);
    assert_eq!(out.map(f32::to_bits), [0; 4]);
    assert_eq!(
        credits,
        [Credit {
            origin: 7,
            bytes: 50
        }]
    );
}

#[test]
fn malformed_input_reports_disconnected_instead_of_staying_full() {
    let (worker, mut input, port, _requests, _reply) = fake();
    let owner = thread::spawn(move || {
        worker.serve(&port, Instant::now(), |_| {
            panic!("malformed ingress does not earn credit");
        });
    });
    input.worker = owner.thread().clone();
    input.submit(packet(7, 1, 20, Some(20))).unwrap();
    owner.join().unwrap();
    assert!(matches!(
        input.submit(packet(8, 0, 1, Some(1))),
        Err(TrySendError::Disconnected(_))
    ));
}

#[test]
fn xrun_refills_an_ended_tail_without_new_ingress_or_credit() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut grant = |_| panic!("already admitted PCM must not emit new ingress credit");

    input.command(Command::Expect).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    for seq in 0..3 {
        let last = (seq == 2).then_some(4_800);
        assert_eq!(
            worker.engine.offer(packet(1, seq, 1_600, last), INSTANT),
            Ok(Admission::Return(Credit {
                origin: 1,
                bytes: 50
            }))
        );
    }
    let (transport, mut playback) = FakePlayback::open(control, &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    assert_eq!(worker.engine.offered, 3_840);
    playback.xrun();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();

    for block in 0..10 {
        let start = Duration::from_millis(block * 21);
        let mut out = [0.0; 480];
        playback.fill(&mut out, start);
        assert_eq!(
            out.iter()
                .filter(|sample| sample.to_bits() == 0.5f32.to_bits())
                .count(),
            480,
            "all frames of block {block} must play"
        );
        worker
            .step(
                &port,
                &mut pcm,
                &mut grant,
                start + Duration::from_millis(21),
            )
            .unwrap();
    }
    assert_eq!(worker.engine.offered, 4_800);
    assert_eq!(worker.engine.progress.heard, 4_800);
    assert_eq!(worker.engine.progress.consumed, 4_800);
    assert_eq!(worker.engine.queue.completed, 1);
}

#[test]
fn a_live_callback_refreshes_the_deadline_until_callbacks_cease() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut grant = |credit| {
        assert_eq!(
            credit,
            Credit {
                origin: 7,
                bytes: 50
            }
        );
    };
    input.command(Command::Expect).unwrap();
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    let (transport, mut playback) = FakePlayback::open(Arc::clone(&control), &OCCUPIED);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker.step(&port, &mut pcm, &mut grant, INSTANT).unwrap();
    assert_eq!(worker.deadline(INSTANT), Some(STALL));
    playback.fill(
        &mut [0.0; 0],
        STALL.checked_sub(Duration::from_millis(1)).unwrap(),
    );
    worker.step(&port, &mut pcm, &mut grant, STALL).unwrap();
    assert_eq!(control.fenced_handoff(), None);
    assert_eq!(worker.deadline(STALL), Some(STALL * 2));

    worker.step(&port, &mut pcm, &mut grant, STALL * 2).unwrap();
    assert_eq!(control.fenced_handoff(), Some(0));
    worker
        .step(
            &port,
            &mut pcm,
            &mut grant,
            STALL * 2 + Duration::from_millis(20),
        )
        .unwrap();
    assert_eq!(worker.engine.progress.heard, 0);
    assert_eq!(worker.engine.progress.consumed, 480);
}

fn staged_for_drop(occupied: &'static AtomicBool) -> (Worker, FakePlayback, Arc<Control>) {
    let (mut worker, input, port, requests, reply) = fake();
    let mut pcm = [0; 480];
    let mut credits = Vec::new();
    input.command(Command::Expect).unwrap();
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    worker
        .step(&port, &mut pcm, &mut |credit| credits.push(credit), INSTANT)
        .unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected an open");
    };
    let (transport, playback) = FakePlayback::open(Arc::clone(&control), occupied);
    assert!(
        reply
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker
        .step(&port, &mut pcm, &mut |credit| credits.push(credit), INSTANT)
        .unwrap();
    assert_eq!(
        credits,
        [Credit {
            origin: 7,
            bytes: 50
        }]
    );
    assert_eq!(worker.engine.offered, 480);
    (worker, playback, control)
}

#[test]
fn a_hold_requested_after_staging_cannot_uncommit_a_line() {
    use super::gate::DropOutcome;

    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let id = SegmentId(0);
    let (mut worker, mut playback, control) = staged_for_drop(&OCCUPIED);
    worker.engine.pause(1, INSTANT);
    worker.inhibit();
    worker.engine.hold(Some(id), INSTANT);
    assert_eq!(worker.drop_unsubmitted(id, INSTANT), DropOutcome::Kept);
    assert_eq!(control.fenced_handoff(), None);

    let later = Duration::from_secs(4);
    let mut out = [99.0; 8];
    playback.fill(&mut out, later);
    assert_eq!(out.map(f32::to_bits), [0; 8]);
    worker.engine.resume(1, later);
    worker.inhibit();
    playback.fill(&mut out, later);
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 8]);
}

#[test]
fn a_drop_after_handoff_keeps_playing_without_waiting_for_its_report() {
    use super::gate::DropOutcome;

    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, mut playback, control) = staged_for_drop(&OCCUPIED);
    let mut out = [99.0; 8];
    playback.fill(&mut out, INSTANT);
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 8]);
    assert_eq!(worker.engine.progress.consumed, 0);
    assert_eq!(
        worker.drop_unsubmitted(SegmentId(0), INSTANT),
        DropOutcome::Kept
    );
    assert_eq!(control.fenced_handoff(), None);
    assert!(!worker.engine.queue.discarded(SegmentId(0)));
    playback.fill(&mut out, INSTANT);
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 8]);
}

#[test]
fn a_hold_before_staging_keeps_a_successor_droppable_while_its_predecessor_plays() {
    use super::gate::DropOutcome;

    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let id = SegmentId(1);
    let (mut worker, mut playback, control) = staged_for_drop(&OCCUPIED);
    worker.engine.hold(Some(id), INSTANT);
    let mut successor = packet(8, 0, 480, Some(480));
    successor.pcm.segment = id;
    worker.engine.offer(successor, INSTANT).unwrap();
    let mut out = [99.0; 8];
    playback.fill(&mut out, INSTANT);
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 8]);

    worker.feed(&mut [0; 480], INSTANT);
    assert_eq!(worker.engine.offered, 480);
    assert_eq!(worker.drop_unsubmitted(id, INSTANT), DropOutcome::Discarded);
    assert_eq!(control.fenced_handoff(), None);

    let mut remainder = [99.0; 480];
    playback.fill(&mut remainder, Duration::from_millis(1));
    assert_eq!(
        remainder.map(f32::to_bits),
        std::array::from_fn(|i| if i < 472 { 0.5f32.to_bits() } else { 0 })
    );
    let now = Duration::from_millis(25);
    worker.observations(now);
    assert_eq!(worker.engine.tick(now), Ok(None));
    assert_eq!(worker.engine.progress.consumed, 960);
    assert_eq!(worker.engine.progress.heard, 480);
    assert_eq!(worker.engine.progress.first_gap, Some(480));
}

fn assert_staging_survives_route_loss(occupied: &'static AtomicBool, copied: usize) {
    use super::gate::DropOutcome;

    let id = SegmentId(0);
    let (mut worker, mut playback, control) = staged_for_drop(occupied);
    playback.fill(&mut [0.0; 8][..copied], INSTANT);
    worker.recover(INSTANT);
    let expected_handoff = u64::try_from(copied).unwrap();
    assert_eq!(control.fenced_handoff(), Some(expected_handoff));
    worker.drive(super::super::super::DRAIN);
    assert!(worker.physical.is_none());
    assert_eq!(worker.engine.offered, expected_handoff);

    assert_eq!(
        worker.drop_unsubmitted(id, super::super::super::DRAIN),
        DropOutcome::Kept
    );
    assert_eq!(worker.engine.queue.samples.len(), 480 - copied);
    assert_eq!(worker.engine.progress.heard, 0);
}

#[test]
fn staging_survives_route_loss_before_any_copy() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    assert_staging_survives_route_loss(&OCCUPIED, 0);
}

#[test]
fn staging_survives_route_loss_after_a_partial_copy() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    assert_staging_survives_route_loss(&OCCUPIED, 8);
}

#[test]
fn virtual_consumption_never_commits_a_segment() {
    use super::gate::DropOutcome;

    let (mut worker, _input, _port, _requests, _reply) = fake();
    worker
        .engine
        .offer(packet(7, 0, 480, Some(480)), INSTANT)
        .unwrap();
    let now = Duration::from_millis(10);
    assert_eq!(worker.engine.tick(now), Ok(None));
    assert_eq!(worker.engine.progress.consumed, 240);
    assert_eq!(
        worker.drop_unsubmitted(SegmentId(0), now),
        DropOutcome::Discarded
    );
    assert_eq!(worker.engine.progress.consumed, 480);
    assert_eq!(worker.engine.progress.heard, 0);
}
