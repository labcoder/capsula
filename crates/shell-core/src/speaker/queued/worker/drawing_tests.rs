use std::sync::Mutex;

use kumochi_protocol::message::{SceneId, SpeechRetirement, StepRef};

use super::*;
use crate::speaker::conductor::{
    Assignment, Motion, Picture, PictureLevel, PictureState, RouteStamp, View, ViewLevel,
};

const SESSION: u64 = 6;

#[derive(Default)]
struct Levels {
    view: Option<ViewLevel>,
    torn: bool,
    picture: Option<PictureLevel>,
    writes: usize,
    exits: Vec<OwnerExit>,
}

#[derive(Clone, Default)]
pub(super) struct FakeIo(Arc<Mutex<Levels>>);

impl DrawingIo for FakeIo {
    fn view(&mut self) -> Result<Option<ViewLevel>, ()> {
        let levels = self.0.lock().unwrap();
        if levels.torn {
            Err(())
        } else {
            Ok(levels.view)
        }
    }

    fn picture(&mut self, key: Binding, level: Option<PictureLevel>) {
        let mut levels = self.0.lock().unwrap();
        if level.is_some() || levels.picture.is_some_and(|old| old.binding == key) {
            levels.picture = level;
            levels.writes += 1;
        }
    }

    fn owner_exited(&mut self, exit: OwnerExit) {
        self.0.lock().unwrap().exits.push(exit);
    }
}

impl FakeIo {
    pub(super) fn attach(worker: &mut Worker, port: &Port) -> Self {
        let io = Self::default();
        worker.drawing = Some(Drawing::new(SESSION, Arc::clone(&port.closed), io.clone()));
        io
    }

    fn view(&self, nonce: u64, segment: u32, view: View) -> Binding {
        let key = Binding {
            assignment: Assignment {
                session: SESSION,
                output: OUTPUT,
                run: RUN,
                segment: SegmentId(segment),
                step: StepRef {
                    scene: SceneId(8),
                    index: segment,
                },
            },
            nonce,
        };
        self.0.lock().unwrap().view = Some(ViewLevel {
            binding: key,
            speech: true,
            view,
        });
        key
    }

    fn latest(&self) -> Option<PictureLevel> {
        self.0.lock().unwrap().picture
    }

    fn picture(&self, key: Binding, route: RouteStamp, state: PictureState) {
        assert_eq!(
            self.latest(),
            Some(PictureLevel {
                binding: key,
                route,
                state
            })
        );
    }

    fn writes(&self) -> usize {
        self.0.lock().unwrap().writes
    }

    pub(super) fn exits(&self) -> Vec<OwnerExit> {
        self.0.lock().unwrap().exits.clone()
    }
}

const fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn step(worker: &mut Worker, port: &Port, now: Duration) {
    assert!(!worker.step(port, &mut [0; 480], &mut |_| {}, now).unwrap());
}

#[test]
fn preparing_installs_the_hold_before_feeding_and_expires_without_packets() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    input.command(Command::Expect).unwrap();
    step(&mut worker, &port, ms(0));
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected an open");
    };
    let (transport, mut playback) = FakePlayback::open(control, &OCCUPIED);
    assert!(
        replies
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );

    let key = io.view(1, 0, View::Preparing);
    input.submit(packet(9, 0, 480, Some(480))).unwrap();
    io.0.lock().unwrap().torn = true;
    step(&mut worker, &port, ms(30));
    assert_eq!(io.writes(), 0);
    assert_eq!(worker.engine.offered, 0);
    io.0.lock().unwrap().torn = false;
    step(&mut worker, &port, ms(30));
    assert_eq!(worker.engine.offered, 0);
    assert!(!worker.engine.queue.was_staged(SegmentId(0)));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::Wait),
    );
    assert_eq!(worker.deadline(ms(30)), Some(ms(280)));

    step(&mut worker, &port, ms(279));
    let mut out = [99.0; 8];
    playback.fill(&mut out, ms(279));
    assert_eq!(out.map(f32::to_bits), [0; 8]);
    step(&mut worker, &port, ms(280));
    assert_eq!(worker.engine.offered, 480);
    assert!(worker.engine.queue.was_staged(SegmentId(0)));
    assert_ne!(worker.deadline(ms(280)), Some(ms(280)));
    io.view(1, 0, View::Eligible);
    step(&mut worker, &port, ms(281));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::Silent),
    );
    playback.fill(&mut out, ms(281));
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 8]);

    input.command(Command::Stop).unwrap();
    step(&mut worker, &port, ms(282));
    io.picture(
        key,
        RouteStamp::Retired(generation),
        PictureState::Owned(Picture::Fenced),
    );
    let writes = io.writes();
    playback.fill(&mut [0.0; 128], ms(283));
    step(&mut worker, &port, ms(284));
    assert!(worker.physical.is_none());
    assert_eq!(io.writes(), writes);
    io.picture(
        key,
        RouteStamp::Retired(generation),
        PictureState::Owned(Picture::Fenced),
    );
}

#[test]
fn no_pcm_has_no_deadline_or_synthetic_terminal() {
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Eligible);
    step(&mut worker, &port, ms(0));
    assert_eq!(worker.deadline(ms(0)), None);
    step(&mut worker, &port, ms(7_000));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Wait),
    );
    assert_eq!(io.writes(), 1);
    input.command(Command::Revoke(key)).unwrap();
    step(&mut worker, &port, ms(7_000));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Ended(Picture::Silent),
    );
}

#[test]
fn completion_before_eligibility_latches_the_end_not_the_wait() {
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Preparing);
    input.submit(packet(3, 0, 0, Some(0))).unwrap();
    step(&mut worker, &port, ms(10));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Ended(Picture::Wait),
    );
    let writes = io.writes();
    io.0.lock().unwrap().torn = true;
    step(&mut worker, &port, ms(11));
    assert_eq!(io.writes(), writes);
    io.0.lock().unwrap().torn = false;
    io.view(1, 0, View::Eligible);
    step(&mut worker, &port, ms(12));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Ended(Picture::Silent),
    );
    io.view(1, 0, View::Shown);
    step(&mut worker, &port, ms(13));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Ended(Picture::Shown(Motion::Authored)),
    );
    io.view(1, 0, View::Fenced);
    step(&mut worker, &port, ms(14));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Fenced),
    );
}

fn refresh(worker: &mut Worker, now: Duration) {
    assert!(worker.read_drawing());
    worker.update_drawing(now);
}

fn physical(worker: &mut Worker, generation: u64, occupied: &'static AtomicBool) -> FakePlayback {
    let control = Arc::new(Control::new(thread::current()));
    let (transport, playback) = FakePlayback::open(control, occupied);
    worker.physical = Some(Physical {
        generation,
        transport,
        flushed: None,
        power: Power::Playing,
        retire_deadline: None,
        watchdog: None,
    });
    playback
}

fn handoff(worker: &mut Worker, generation: u64, now: Duration, at: Duration) {
    worker.drawing.as_mut().unwrap().observe(
        Anchor {
            run: RUN,
            generation,
            segment: SegmentId(0),
            first: 8,
            through: 9,
            at,
        },
        now,
    );
}

#[test]
fn onset_uses_the_first_handoff_and_shown_stays_authored_across_a_coalesced_retirement() {
    static OLD: AtomicBool = AtomicBool::new(false);
    static NEW: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Eligible);
    worker.engine.expect(ms(0));
    let generation = worker.engine.request(ms(0)).unwrap();
    assert!(worker.engine.opened(generation, Opened::Physical, ms(0)));
    let mut playback = physical(&mut worker, generation, &OLD);
    input.submit(packet(5, 0, 1_920, Some(1_920))).unwrap();
    step(&mut worker, &port, ms(0));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::Wait),
    );
    playback.fill(&mut [0.0; 8], ms(100));
    step(&mut worker, &port, ms(10));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::At(ms(100))),
    );

    io.view(1, 0, View::Shown);
    step(&mut worker, &port, ms(100));
    let authored = PictureState::Owned(Picture::Shown(Motion::Authored));
    io.picture(key, RouteStamp::Live(generation), authored);
    let shown_writes = io.writes();
    playback.fill(&mut [0.0; 8], ms(101));
    step(&mut worker, &port, ms(103));
    assert_eq!(io.writes(), shown_writes);

    worker.recover(ms(105));
    refresh(&mut worker, ms(105));
    let newer = generation + 1;
    let _replacement = physical(&mut worker, newer, &NEW);
    handoff(&mut worker, newer, ms(107), ms(110));
    refresh(&mut worker, ms(107));
    assert_eq!(io.writes(), shown_writes + 2);
    io.picture(key, RouteStamp::Live(newer), authored);
}

#[test]
fn an_unshown_rebuild_expires_without_updates_and_cannot_reanchor_at_equality() {
    static ROUTES: [AtomicBool; 6] = [const { AtomicBool::new(false) }; 6];
    for (index, (arrival, expected)) in [
        (Some(ms(279)), Picture::At(ms(300))),
        (Some(ms(280)), Picture::Silent),
        (None, Picture::Silent),
    ]
    .into_iter()
    .enumerate()
    {
        let (mut worker, _input, port, _requests, _replies) = fake();
        let io = FakeIo::attach(&mut worker, &port);
        let key = io.view(1, 0, View::Eligible);
        let _old = physical(&mut worker, 7, &ROUTES[index * 2]);
        refresh(&mut worker, ms(0));
        worker.recover(ms(30));
        refresh(&mut worker, ms(30));
        io.picture(
            key,
            RouteStamp::Retired(7),
            PictureState::Owned(Picture::Wait),
        );
        assert_eq!(worker.deadline(ms(30)), Some(ms(280)));
        refresh(&mut worker, ms(278));
        if let Some(now) = arrival {
            let _new = physical(&mut worker, 8, &ROUTES[index * 2 + 1]);
            handoff(&mut worker, 8, now, ms(300));
            refresh(&mut worker, now);
            io.picture(key, RouteStamp::Live(8), PictureState::Owned(expected));
        } else {
            refresh(&mut worker, ms(280));
            io.picture(key, RouteStamp::Retired(7), PictureState::Owned(expected));
        }
        assert_eq!(worker.drawing.as_ref().unwrap().deadline(), None);
    }
}

#[test]
fn replacement_rejects_old_terminals_and_latches_a_current_terminal_during_a_torn_read() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let old = io.view(1, 0, View::Eligible);
    let _playback = physical(&mut worker, 7, &OCCUPIED);
    refresh(&mut worker, ms(0));
    worker.recover(ms(5));

    let key = io.view(2, 1, View::Eligible);
    input.command(Command::Revoke(old)).unwrap();
    step(&mut worker, &port, ms(5));
    io.picture(
        key,
        RouteStamp::Retired(7),
        PictureState::Owned(Picture::Wait),
    );
    let collision = io.view(2, 2, View::Fenced);
    assert_ne!(collision, key);
    input.command(Command::Revoke(collision)).unwrap();
    step(&mut worker, &port, ms(6));
    io.picture(
        key,
        RouteStamp::Retired(7),
        PictureState::Owned(Picture::Wait),
    );

    let latest = io.view(3, 2, View::Eligible);
    io.0.lock().unwrap().torn = true;
    input.command(Command::Revoke(latest)).unwrap();
    step(&mut worker, &port, ms(7));
    io.picture(
        key,
        RouteStamp::Retired(7),
        PictureState::Owned(Picture::Wait),
    );
    io.0.lock().unwrap().torn = false;
    step(&mut worker, &port, ms(8));
    io.picture(
        latest,
        RouteStamp::Retired(7),
        PictureState::Ended(Picture::Silent),
    );

    io.0.lock().unwrap().view = None;
    step(&mut worker, &port, ms(9));
    assert_eq!(io.latest(), None);
    io.view(3, 2, View::Eligible);
    step(&mut worker, &port, ms(10));
    assert_eq!(io.latest(), None);
}

#[test]
fn failed_backend_start_and_idle_exit_notify_without_an_ingress_packet() {
    for fail in [true, false] {
        let (worker, input, port, _requests, _replies) = fake();
        let io = FakeIo::default();
        let drawing = Drawing::new(SESSION, Arc::clone(&port.closed), io.clone());
        let mut input = Some(input);
        if !fail {
            drop(input.take());
        }
        let origin = Instant::now().checked_sub(ms(37)).unwrap();
        let mut reports = Vec::new();
        own(
            &port,
            worker.engine,
            origin,
            drawing,
            |_| panic!("no credit"),
            |report| {
                reports.push(report);
                true
            },
            |_, provided| {
                assert_eq!(provided, origin);
                if fail {
                    Err(io::ErrorKind::ResourceBusy.into())
                } else {
                    Ok((worker.send, worker.replies))
                }
            },
        );
        assert!(port.closed.load(Ordering::Acquire));
        assert_eq!(io.exits(), [OwnerExit::Closed]);
        assert_eq!(io.writes(), 0);
        if fail {
            assert!(reports.is_empty());
        } else {
            assert_eq!(
                reports,
                [ControlToBrain::SpeechStopped {
                    run: RUN,
                    cursor: None,
                }]
            );
        }
    }
}

#[test]
fn refused_ordered_delivery_closes_ingress_and_preserves_only_the_accepted_prefix() {
    let mut ordered: Vec<_> = (0..3)
        .map(|id| {
            ControlToBrain::SpeechRetired(SpeechRetirement {
                run: RUN,
                output: OUTPUT,
                segment: SegmentId(id),
                retired: 480,
                heard: 0,
                final_segment: true,
            })
        })
        .collect();
    ordered.push(ControlToBrain::SpeechStopped {
        run: RUN,
        cursor: None,
    });
    for (capacity, stopping) in [(1, false), (1, true), (3, true)] {
        let (mut worker, input, port, requests, _replies) = fake();
        let io = FakeIo::attach(&mut worker, &port);
        for id in 0..3 {
            let mut line = packet(7, 0, 480, Some(480));
            line.pcm.segment = SegmentId(id);
            worker.engine.offer(line, ms(u64::from(id) * 20)).unwrap();
            worker.engine.tick(ms(u64::from(id + 1) * 20)).unwrap();
        }
        if stopping {
            input.command(Command::Stop).unwrap();
        }
        if capacity == 3 {
            port.closed.store(true, Ordering::Release);
        }
        let (send, received) = mpsc::sync_channel(capacity);
        let mut attempts = 0;
        worker.serve(
            &port,
            Instant::now().checked_sub(ms(60)).unwrap(),
            |_| panic!("no ingress credit is owed"),
            |report| {
                assert!(attempts <= capacity, "report attempted after refusal");
                assert_eq!(report, ordered[attempts]);
                attempts += 1;
                send.try_send(report).is_ok()
            },
        );
        assert_eq!(attempts, capacity + 1);
        assert_eq!(received.try_iter().collect::<Vec<_>>(), ordered[..capacity]);
        assert_eq!(io.exits(), [OwnerExit::ReportRefused]);
        assert!(matches!(
            input.command(Command::Stop),
            Err(TrySendError::Disconnected(Command::Stop))
        ));
        assert!(matches!(
            input.submit(packet(8, 0, 1, Some(1))),
            Err(TrySendError::Disconnected(_))
        ));
        if !stopping {
            let Request::Open(_, control) = requests.try_recv().unwrap() else {
                panic!("expected the pending open to be fenced");
            };
            assert_eq!(control.fenced_handoff(), Some(0));
        }
    }
}

#[test]
fn route_loss_retires_a_whole_copied_line_without_ending_its_binding() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Eligible);
    worker.engine.expect(ms(0));
    let generation = worker.engine.request(ms(0)).unwrap();
    assert!(worker.engine.opened(generation, Opened::Physical, ms(0)));
    let mut playback = physical(&mut worker, generation, &OCCUPIED);

    input.submit(packet(17, 0, 480, Some(480))).unwrap();
    step(&mut worker, &port, ms(0));
    let mut out = [0.0; 480];
    playback.fill(&mut out, ms(100));
    assert_eq!(out.map(f32::to_bits), [0.5f32.to_bits(); 480]);
    step(&mut worker, &port, ms(10));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::At(ms(100))),
    );

    worker.recover(ms(20));
    step(&mut worker, &port, ms(20));
    assert_eq!(worker.engine.progress.consumed, 480);
    assert_eq!(worker.engine.progress.heard, 0);
    assert_eq!(worker.engine.queue.completed, 1);
    playback.fill(&mut [0.0; 128], ms(21));
    step(&mut worker, &port, ms(22));
    assert!(worker.physical.is_none());
    let stamp = RouteStamp::Retired(generation);
    io.picture(key, stamp, PictureState::Owned(Picture::Wait));
    assert_eq!(worker.deadline(ms(22)), Some(ms(270)));
    step(&mut worker, &port, ms(269));
    io.picture(key, stamp, PictureState::Owned(Picture::Wait));
    step(&mut worker, &port, ms(270));
    io.picture(key, stamp, PictureState::Owned(Picture::Silent));
}

#[test]
fn a_confirmed_line_after_an_earlier_virtual_gap_can_end_its_binding() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, _requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    worker.engine.expect(ms(0));
    let generation = worker.engine.request(ms(0)).unwrap();
    worker
        .engine
        .offer(packet(17, 0, 480, Some(480)), ms(0))
        .unwrap();
    assert_eq!(worker.engine.tick(ms(20)), Ok(None));
    assert_eq!(worker.engine.progress.first_gap, Some(0));
    assert!(worker.engine.opened(generation, Opened::Physical, ms(20)));
    let mut playback = physical(&mut worker, generation, &OCCUPIED);
    let key = io.view(1, 1, View::Eligible);
    let mut successor = packet(18, 0, 480, Some(480));
    successor.pcm.segment = SegmentId(1);
    input.submit(successor).unwrap();
    step(&mut worker, &port, ms(20));
    playback.fill(&mut [0.0; 480], ms(30));
    step(&mut worker, &port, ms(50));
    assert_eq!(worker.engine.progress.consumed, 960);
    assert_eq!(worker.engine.progress.heard, 0);
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Ended(Picture::Silent),
    );
}

#[test]
fn a_pending_open_arms_the_hold_for_ready_pcm_and_releases_it_to_virtual_pacing() {
    let (mut worker, input, port, requests, _replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Preparing);
    input.command(Command::Expect).unwrap();
    step(&mut worker, &port, ms(0));
    assert!(matches!(requests.try_recv(), Ok(Request::Open(_, _))));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Wait),
    );

    input.submit(packet(17, 0, 480, Some(480))).unwrap();
    step(&mut worker, &port, ms(30));
    assert_eq!(worker.deadline(ms(30)), Some(ms(280)));
    step(&mut worker, &port, ms(279));
    assert_eq!(worker.engine.progress.consumed, 0);
    step(&mut worker, &port, ms(280));
    assert_eq!(worker.engine.progress.consumed, 0);
    assert_eq!(worker.deadline(ms(280)), Some(ms(300)));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Wait),
    );

    io.view(1, 0, View::Eligible);
    step(&mut worker, &port, ms(290));
    assert_eq!(worker.engine.progress.consumed, 240);
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Silent),
    );
    step(&mut worker, &port, ms(300));
    assert_eq!(worker.engine.progress.consumed, 480);
    assert_eq!(worker.engine.progress.heard, 0);
    assert!(!worker.engine.queue.was_staged(SegmentId(0)));
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Silent),
    );
    let next = io.view(2, 1, View::Eligible);
    let mut successor = packet(18, 0, 480, Some(480));
    successor.pcm.segment = SegmentId(1);
    input.submit(successor).unwrap();
    step(&mut worker, &port, ms(300));
    io.picture(
        next,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Wait),
    );
    step(&mut worker, &port, ms(310));
    assert_eq!(worker.engine.progress.consumed, 720);
    io.picture(
        next,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Silent),
    );
}

#[test]
fn opening_after_the_eligible_line_is_consumed_uses_silent_fallback_without_ending() {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    let (mut worker, input, port, requests, replies) = fake();
    let io = FakeIo::attach(&mut worker, &port);
    let key = io.view(1, 0, View::Preparing);
    input.command(Command::Expect).unwrap();
    step(&mut worker, &port, ms(0));
    let Request::Open(generation, control) = requests.try_recv().unwrap() else {
        panic!("expected an open");
    };
    input.submit(packet(9, 0, 240, Some(240))).unwrap();
    step(&mut worker, &port, ms(30));
    assert_eq!(worker.deadline(ms(30)), Some(ms(280)));
    io.view(1, 0, View::Eligible);
    step(&mut worker, &port, ms(31));
    assert_eq!(worker.engine.progress.consumed, 0);
    io.picture(
        key,
        RouteStamp::Unstamped,
        PictureState::Owned(Picture::Wait),
    );

    let (transport, _playback) = FakePlayback::open(control, &OCCUPIED);
    assert!(
        replies
            .send(Reply::Opened(generation, Some(transport)))
            .is_ok()
    );
    step(&mut worker, &port, ms(42));
    assert_eq!(worker.engine.progress.consumed, 240);
    assert_eq!(worker.engine.progress.heard, 0);
    assert!(!worker.engine.queue.was_staged(SegmentId(0)));
    io.picture(
        key,
        RouteStamp::Live(generation),
        PictureState::Owned(Picture::Silent),
    );
}
