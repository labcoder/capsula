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
            closed: Arc::clone(&closed),
        },
        Port {
            commands: pending,
            packets: data,
            busy,
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
