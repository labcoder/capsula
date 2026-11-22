use super::*;
use crate::platform::speaker_cpal::tests::FakePlayback;
use kumochi_protocol::message::{Cursor, SpeechPcm, SpeechRetirement};

pub(in crate::speaker::queued) const RUN: RunId = RunId(17);
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
            retired_route: None,
            stop_cutoff: None,
            virtual_route: false,
            drawing: None,
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

pub(in crate::speaker::queued) struct OpenActor {
    pub input: Input,
    pub requests: Receiver<Request>,
    pub replies: SyncSender<Reply>,
    pub owner: thread::JoinHandle<()>,
    pub done: Receiver<()>,
}

pub(in crate::speaker::queued) fn open_actor(
    report: impl FnMut(ControlToBrain) -> bool + Send + 'static,
) -> OpenActor {
    let (worker, mut input, port, requests, replies) = fake();
    let (exit, done) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        worker.serve(&port, Instant::now(), |_| {}, report, None::<fn(bool)>);
        exit.send(()).unwrap();
    });
    input.worker = owner.thread().clone();
    OpenActor {
        input,
        requests,
        replies,
        owner,
        done,
    }
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

fn stage_receipts(
    worker: &mut Worker,
    input: &Input,
    port: &Port,
    requests: &Receiver<Request>,
    replies: &SyncSender<Reply>,
    occupied: &'static AtomicBool,
) -> (FakePlayback, Arc<Control>) {
    input.command(Command::Expect).unwrap();
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    worker
        .step(port, &mut [0; 480], &mut |_| {}, INSTANT)
        .unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected a pending open");
    };
    let (transport, playback) = FakePlayback::open(Arc::clone(&control), occupied);
    assert!(
        replies
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker
        .step(port, &mut [0; 480], &mut |_| {}, INSTANT)
        .unwrap();
    (playback, control)
}

#[test]
fn a_full_presentation_timeline_drains_at_its_deadline_without_new_callbacks() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, replies) = fake();
    let (mut playback, control) =
        stage_receipts(&mut worker, &input, &port, &requests, &replies, &OCCUPIED);
    for time in 20..=36 {
        playback.fill(&mut [0.0], Duration::from_millis(time));
        worker
            .step(&port, &mut [0; 480], &mut |_| {}, INSTANT)
            .unwrap();
    }
    assert_eq!(worker.engine.progress.heard, 0);
    assert!(
        !control.stopped(),
        "a full timeline doesn't invalidate the physical route"
    );
    assert_eq!(worker.deadline(INSTANT), Some(Duration::from_millis(20)));
    worker
        .step(&port, &mut [0; 480], &mut |_| {}, Duration::from_millis(20))
        .unwrap();
    assert_eq!(worker.engine.progress.heard, 1);
    assert_eq!(
        worker.deadline(Duration::from_millis(20)),
        Some(Duration::from_millis(21))
    );
    worker
        .step(&port, &mut [0; 480], &mut |_| {}, Duration::from_millis(36))
        .unwrap();
    assert_eq!(worker.engine.progress.heard, 17);
    assert_eq!(worker.engine.progress.first_gap, None);
    assert!(!control.stopped());
}

#[test]
fn stop_waits_for_the_last_publication_and_uses_the_original_cutoff() {
    const CUTOFF: Duration = Duration::from_millis(10);
    static OCCUPIED: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];
    for ((last_at, due), occupied) in [(9, 8), (11, 7)].into_iter().zip(&OCCUPIED) {
        let (mut worker, input, port, requests, replies) = fake();
        let (mut playback, control) =
            stage_receipts(&mut worker, &input, &port, &requests, &replies, occupied);
        for time in 0..7 {
            playback.fill(&mut [0.0], Duration::from_millis(time));
        }
        let mut receipts = Receipts::default();
        let mut reports = Vec::new();
        let mut last = [0.0];
        playback.fill_before_publish(&mut last, Duration::from_millis(last_at), || {
            assert!(input.stop());
            worker
                .step(&port, &mut [0; 480], &mut |_| {}, CUTOFF)
                .unwrap();
            assert_eq!(control.fenced_handoff(), Some(8));
            assert_eq!(control.tail(), TailAck::Pending);
            assert!(receipts.publish(&worker.engine, CUTOFF, &mut |report| {
                reports.push(report);
                true
            }));
            assert!(
                reports.is_empty(),
                "the terminal report cannot overtake publication"
            );
        });
        assert_eq!(last.map(f32::to_bits), [0.5f32.to_bits()]);
        let fade_at = Duration::from_millis(12);
        let mut fade = [99.0; 120];
        playback.fill(&mut fade, fade_at);
        assert!(
            fade[..119]
                .iter()
                .all(|sample| *sample > 0.0 && *sample < 0.5)
        );
        assert_eq!(fade[119].to_bits(), 0);
        let TailAck::At(quiet) = control.tail() else {
            panic!("fade should publish quiet even with the receipt ring full");
        };
        worker
            .step(&port, &mut [0; 480], &mut |_| {}, quiet)
            .unwrap();
        assert!(receipts.publish(&worker.engine, quiet, &mut |report| {
            reports.push(report);
            true
        }));
        let cursor = Cursor {
            segment: SegmentId(0),
            sample: due,
        };
        assert_eq!(
            reports,
            [
                ControlToBrain::SpeechRetired(SpeechRetirement {
                    run: RUN,
                    output: OUTPUT,
                    segment: cursor.segment,
                    retired: due,
                    heard: due,
                    final_segment: false,
                }),
                ControlToBrain::SpeechStopped {
                    run: RUN,
                    cursor: Some(cursor)
                },
            ]
        );
    }
}

#[test]
fn stop_is_processed_while_the_open_and_ingress_mailboxes_are_busy() {
    let (worker, mut input, port, requests, reply) = fake();
    let (credits, received) = mpsc::sync_channel(1);
    let (closed, done) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        worker.serve(
            &port,
            Instant::now(),
            |credit| {
                credits.send(credit).unwrap();
            },
            |_| true,
            None::<fn(bool)>,
        );
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
fn dropping_input_keeps_a_worker_alive_until_its_known_quiet_deadline() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, replies) = fake();
    let io = drawing_tests::FakeIo::attach(&mut worker, &port);
    let mut source = [0; 480];
    input.command(Command::Expect).unwrap();
    input.submit(packet(7, 0, 480, Some(480))).unwrap();
    worker
        .step(&port, &mut source, &mut |_| {}, INSTANT)
        .unwrap();
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected an open");
    };
    let (transport, mut playback) = FakePlayback::open(Arc::clone(&control), &OCCUPIED);
    assert!(
        replies
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    worker
        .step(&port, &mut source, &mut |_| {}, INSTANT)
        .unwrap();
    playback.fill(&mut [0.0; 1], INSTANT);
    let stop = Duration::from_millis(1);
    input.command(Command::Stop).unwrap();
    worker.step(&port, &mut source, &mut |_| {}, stop).unwrap();
    drop(input);

    let origin = Instant::now();
    playback.fill(&mut [0.0; 128], Duration::from_millis(200));
    let TailAck::At(quiet_at) = control.tail() else {
        panic!("the fake proved quiet")
    };
    assert!(origin.elapsed() < quiet_at);
    let mut reports = Vec::new();
    worker.serve(
        &port,
        origin,
        |_| {},
        |message| {
            reports.push((origin.elapsed(), message));
            true
        },
        None::<fn(bool)>,
    );
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(matches!(reports[0].1, ControlToBrain::SpeechRetired(_)));
    assert_eq!(
        reports[1].1,
        ControlToBrain::SpeechStopped {
            run: RUN,
            cursor: Some(kumochi_protocol::message::Cursor {
                segment: SegmentId(0),
                sample: 1
            })
        }
    );
    assert!(reports[1].0 >= quiet_at);
    assert_eq!(io.exits(), [OwnerExit::Closed]);
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
        assert!(
            worker
                .physical
                .as_mut()
                .unwrap()
                .transport
                .presentation()
                .is_some()
        );
    }
    playback.force_observation_overflow();
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
        worker.serve(
            &port,
            Instant::now(),
            |_| {
                panic!("malformed ingress does not earn credit");
            },
            |_| true,
            None::<fn(bool)>,
        );
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

#[path = "drawing_tests.rs"]
mod drawing_tests;
