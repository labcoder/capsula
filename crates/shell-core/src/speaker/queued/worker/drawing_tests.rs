use std::sync::Mutex;

use kumochi_protocol::message::{SceneId, StepRef};

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
    exits: usize,
}

#[derive(Clone, Default)]
struct FakeIo(Arc<Mutex<Levels>>);

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

    fn owner_exited(&mut self) {
        self.0.lock().unwrap().exits += 1;
    }
}

impl FakeIo {
    fn attach(worker: &mut Worker, port: &Port) -> Self {
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

    fn picture(&self, key: Binding, route: RouteStamp, state: PictureState) {
        assert_eq!(
            self.0.lock().unwrap().picture,
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
    assert_eq!(io.0.lock().unwrap().picture, None);
    io.view(3, 2, View::Eligible);
    step(&mut worker, &port, ms(10));
    assert_eq!(io.0.lock().unwrap().picture, None);
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
        own(
            &port,
            worker.engine,
            origin,
            drawing,
            |_| panic!("no credit"),
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
        assert_eq!(io.0.lock().unwrap().exits, 1);
        assert_eq!(io.writes(), 0);
    }
}
