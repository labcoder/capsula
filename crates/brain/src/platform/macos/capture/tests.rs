use std::cell::Cell;

use super::*;

fn admitted(major: Option<u32>, framework: fn() -> bool) -> (Result<(), Failure>, bool) {
    let asked = Cell::new(false);
    let result = admit(
        || major,
        || {
            asked.set(true);
            framework()
        },
    );
    (result, asked.get())
}

#[test]
fn an_older_or_unknown_system_refuses_unsupported_without_loading_the_framework() {
    for major in [Some(13), Some(10), None] {
        assert_eq!(
            admitted(major, || true),
            (Err(Failure::Refused(Refusal::Unsupported)), false)
        );
    }
}

#[test]
fn macos_14_loads_the_framework_and_opens_only_when_it_is_complete() {
    assert_eq!(admitted(Some(14), || true), (Ok(()), true));
    assert_eq!(admitted(Some(26), || true), (Ok(()), true));
    assert_eq!(
        admitted(Some(14), || false),
        (Err(Failure::Refused(Refusal::Unsupported)), true)
    );
}

#[test]
fn the_major_version_reads_from_a_nul_terminated_product_version() {
    assert_eq!(major(b"26.5.1\0"), Some(26));
    assert_eq!(major(b"14.0\0"), Some(14));
    assert_eq!(major(b"14\0"), Some(14));
    assert_eq!(major(b"10.15.7\0junk"), Some(10));
    assert_eq!(major(b"26.5.1"), None);
    assert_eq!(major(b"\0"), None);
    assert_eq!(major(b"x.1\0"), None);
}

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};

use super::chain::{Ended, UNFINISHED};
use super::layout::Screen;

/// Stamps a test marked spent; each test uses its own.
static SPENT: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

fn left(stamp: u64) -> Option<Duration> {
    (!SPENT.lock().unwrap().contains(&stamp)).then_some(Duration::from_secs(60))
}

/// A wake clock frozen before any test deadline, so the callback's deadline checks pass unless
/// the stamp is spent; the capture thread still waits on the real clock.
fn frozen() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Query {
    #[default]
    Answer,
    /// Answers only after the capture thread tried to stop the chain.
    AfterStop,
    Never,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Prepare {
    #[default]
    Ready,
    SpendStamp,
    AfterStop,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Shoot {
    #[default]
    Image,
    Fail(Ended),
    AfterStop,
    Never,
    /// Completes inside the call, then holds the call open until the test lets go.
    InlineThenHold,
}

#[derive(Default)]
struct Fake {
    stamp: u64,
    query: Query,
    prepare: Prepare,
    shoot: Shoot,
    /// Preflight answers in order; once they run out, yes.
    grants: Mutex<VecDeque<bool>>,
    /// The callback's preflight spends the stamp.
    preflight_spends: bool,
    screens: Vec<Screen>,
    preflights: AtomicUsize,
    queries: AtomicUsize,
    prepares: AtomicUsize,
    shots: AtomicUsize,
    released: Arc<AtomicUsize>,
    stopped: Arc<(Mutex<bool>, Condvar)>,
    held: Held,
}

fn wait_stopped((stopped, changed): &(Mutex<bool>, Condvar)) {
    drop(
        changed
            .wait_while(stopped.lock().unwrap(), |stopped| !*stopped)
            .unwrap(),
    );
}

type Held = (
    Mutex<Option<mpsc::Sender<()>>>,
    Mutex<Option<mpsc::Receiver<()>>>,
);

/// The request every fake capture makes; another one fails the fake.
const EXPECTED: Request = Request {
    display: 5,
    points: [4, 3],
    size: [8, 6],
    shell: 9,
};

struct Image(Arc<AtomicUsize>);

impl Drop for Image {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Fake {
    fn new() -> Self {
        frozen();
        Self {
            stamp: NEXT_STAMP.fetch_add(1, Ordering::SeqCst),
            screens: vec![Screen {
                id: 5,
                origin: [0.0, 0.0],
                points: [4.0, 3.0],
                pixels: [8, 6],
                mirrored: false,
            }],
            ..Self::default()
        }
    }

    fn spend(&self) {
        SPENT.lock().unwrap().push(self.stamp);
    }

    fn after_stop(&self) {
        wait_stopped(&self.stopped);
    }

    fn image(&self) -> Image {
        Image(Arc::clone(&self.released))
    }

    fn counts(&self) -> [usize; 4] {
        [&self.preflights, &self.queries, &self.prepares, &self.shots]
            .map(|n| n.load(Ordering::SeqCst))
    }
}

impl Sck for Fake {
    type Content = ();
    type Shot = ();
    type Image = Image;

    fn displays(&self) -> Result<Vec<Screen>, Failure> {
        Ok(self.screens.clone())
    }

    fn preflight(&self) -> bool {
        let first = self.preflights.fetch_add(1, Ordering::SeqCst) == 0;
        if !first && self.preflight_spends {
            self.spend();
        }
        self.grants.lock().unwrap().pop_front().unwrap_or(true)
    }

    fn query(&self, ready: chain::Done<()>) {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.query == Query::Never {
            std::mem::forget(ready);
            return;
        }
        let stopped = (self.query == Query::AfterStop).then(|| Arc::clone(&self.stopped));
        std::thread::spawn(move || {
            if let Some(stopped) = stopped {
                wait_stopped(&stopped);
            }
            ready(Ok(()));
        });
    }

    fn prepare(&self, (): (), request: &Request) -> Result<(), Ended> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        if *request != EXPECTED {
            return Err(Ended::Code {
                sck: false,
                code: -99,
            });
        }
        match self.prepare {
            Prepare::Ready => {}
            Prepare::SpendStamp => self.spend(),
            Prepare::AfterStop => self.after_stop(),
        }
        Ok(())
    }

    fn shoot(&self, (): (), done: chain::Done<Image>) {
        self.shots.fetch_add(1, Ordering::SeqCst);
        match self.shoot {
            Shoot::Image => done(Ok(self.image())),
            Shoot::Fail(ended) => done(Err(ended)),
            Shoot::AfterStop => {
                self.after_stop();
                done(Ok(self.image()));
            }
            Shoot::Never => std::mem::forget(done),
            Shoot::InlineThenHold => {
                done(Ok(self.image()));
                let (signal, gate) = &self.held;
                signal.lock().unwrap().take().unwrap().send(()).unwrap();
                gate.lock().unwrap().take().unwrap().recv().unwrap();
            }
        }
    }

    fn draw(&self, _: &Image, size: [u32; 2], frame: &mut [u8]) -> Result<(), Failure> {
        if size != [8, 6] {
            return Err(Failure::Fault(-99));
        }
        frame.fill(7);
        Ok(())
    }

    fn expired(&self) {
        let (stopped, changed) = &*self.stopped;
        *stopped.lock().unwrap() = true;
        changed.notify_all();
    }
}

const BOUNDS: PhysicalRect = PhysicalRect {
    origin: [0, 0],
    size: [8, 6],
};

/// One display capture at (1, 1) for shell 9 with a nonzero stamp, waiting `until` and `grace`.
fn shot(
    fake: &Arc<Fake>,
    until: Duration,
    grace: Duration,
) -> Result<(Acquired, PhysicalRect), Failure> {
    shot_with(fake, until, grace, 9, 3)
}

fn shot_with(
    fake: &Arc<Fake>,
    until: Duration,
    grace: Duration,
    shell: u32,
    excluded: u64,
) -> Result<(Acquired, PhysicalRect), Failure> {
    let mut capturer = ScreenCaptureKit {
        sck: Arc::clone(fake),
        clock: Clock {
            now: frozen,
            continuous: crate::platform::macos::clock::counter,
            grace,
        },
    };
    let deadline = Deadline::new(Instant::now() + until, fake.stamp, left);
    let mut frame = RawFrame::default();
    capturer.capture_display(PhysicalPoint([1, 1]), shell, excluded, deadline, &mut frame)
}

const LONG: Duration = Duration::from_secs(60);
const SHORT: Duration = Duration::from_millis(30);

#[test]
fn a_display_capture_draws_one_screenshot_and_releases_it() {
    let fake = Arc::new(Fake::new());
    let (acquired, bounds) = shot(&fake, LONG, LONG).expect("a frame");
    assert_eq!((acquired.dpi, bounds), (192, BOUNDS));
    assert_eq!(fake.counts(), [2, 1, 1, 1]);
    assert_eq!(fake.released.load(Ordering::SeqCst), 1);
}

#[test]
fn denial_at_entry_asks_nothing_of_the_framework() {
    let fake = Arc::new(Fake::new());
    fake.grants.lock().unwrap().push_back(false);
    assert_eq!(shot(&fake, LONG, LONG), Err(Refusal::NotPermitted.into()));
    assert_eq!(fake.counts(), [1, 0, 0, 0]);
}

#[test]
fn revocation_after_the_content_query_takes_no_screenshot() {
    let fake = Arc::new(Fake::new());
    fake.grants.lock().unwrap().extend([true, false]);
    assert_eq!(shot(&fake, LONG, LONG), Err(Refusal::NotPermitted.into()));
    assert_eq!(fake.counts(), [2, 1, 1, 0]);
}

#[test]
fn refusals_before_any_request_ask_nothing_of_the_framework() {
    let mut oversized = Fake::new();
    oversized.screens[0].points = [8193.0, 3.0];
    oversized.screens[0].pixels = [8193, 3];
    let oversized = Arc::new(oversized);
    let mut mixed = Fake::new();
    let mut second = mixed.screens[0];
    second.origin = [8.0, 0.0];
    second.pixels = [4, 3];
    mixed.screens.push(second);
    let mixed = Arc::new(mixed);
    let plain = Arc::new(Fake::new());
    let cases = [
        (&oversized, 9, 3, Refusal::Invalid),
        (&mixed, 9, 3, Refusal::Layout),
        (&plain, 0, 3, Refusal::Excluded),
        (&plain, 9, 0, Refusal::Expired),
    ];
    for (fake, shell, excluded, refusal) in cases {
        assert_eq!(
            shot_with(fake, LONG, LONG, shell, excluded),
            Err(refusal.into())
        );
        assert_eq!(fake.counts(), [0, 0, 0, 0]);
    }
}

#[test]
fn expiry_inside_the_callback_submits_nothing() {
    let mut spent_in_prepare = Fake::new();
    spent_in_prepare.prepare = Prepare::SpendStamp;
    let mut spent_by_preflight = Fake::new();
    spent_by_preflight.preflight_spends = true;
    for fake in [spent_in_prepare, spent_by_preflight] {
        let fake = Arc::new(fake);
        assert_eq!(shot(&fake, LONG, LONG), Err(Refusal::Expired.into()));
        assert_eq!(fake.counts(), [2, 1, 1, 0]);
    }
}

#[test]
fn a_content_callback_after_the_stop_submits_nothing() {
    let mut fake = Fake::new();
    fake.query = Query::AfterStop;
    let fake = Arc::new(fake);
    assert_eq!(shot(&fake, SHORT, LONG), Err(Refusal::Expired.into()));
    assert_eq!(fake.counts(), [1, 1, 0, 0]);
}

#[test]
fn a_stop_that_lands_before_the_commit_takes_no_screenshot() {
    let mut fake = Fake::new();
    fake.prepare = Prepare::AfterStop;
    let fake = Arc::new(fake);
    assert_eq!(shot(&fake, SHORT, LONG), Err(Refusal::Expired.into()));
    assert_eq!(fake.counts(), [2, 1, 1, 0]);
}

#[test]
fn a_commit_before_the_stop_waits_for_the_screenshot_or_retires() {
    let mut finishes = Fake::new();
    finishes.shoot = Shoot::AfterStop;
    let finishes = Arc::new(finishes);
    assert_eq!(shot(&finishes, SHORT, LONG), Err(Refusal::Expired.into()));
    assert_eq!(finishes.counts(), [2, 1, 1, 1]);
    assert_eq!(finishes.released.load(Ordering::SeqCst), 1);

    let mut lost = Fake::new();
    lost.shoot = Shoot::Never;
    let lost = Arc::new(lost);
    assert_eq!(shot(&lost, SHORT, SHORT), Err(Failure::Retire(UNFINISHED)));
    assert_eq!(lost.counts(), [2, 1, 1, 1]);
}

#[test]
fn a_content_query_that_never_answers_retires() {
    let mut fake = Fake::new();
    fake.query = Query::Never;
    let fake = Arc::new(fake);
    assert_eq!(shot(&fake, SHORT, SHORT), Err(Failure::Retire(UNFINISHED)));
    assert_eq!(fake.counts(), [1, 1, 0, 0]);
}

#[test]
fn an_inline_screenshot_completion_waits_for_the_content_callback_to_return() {
    let mut fake = Fake::new();
    fake.shoot = Shoot::InlineThenHold;
    let (signal, inline) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    fake.held = (Mutex::new(Some(signal)), Mutex::new(Some(gate)));
    let fake = Arc::new(fake);
    let (answer, answered) = mpsc::channel();
    let capturing = Arc::clone(&fake);
    let capture = std::thread::spawn(move || answer.send(shot(&capturing, LONG, LONG)).unwrap());
    inline.recv().unwrap();
    assert!(answered.recv_timeout(Duration::from_millis(100)).is_err());
    release.send(()).unwrap();
    let (_, bounds) = answered.recv().unwrap().expect("a frame");
    assert_eq!(bounds, BOUNDS);
    capture.join().unwrap();
}

#[test]
fn framework_errors_map_by_domain_and_code_only() {
    let cases = [
        (
            Ended::Code {
                sck: true,
                code: -3801,
            },
            Failure::Refused(Refusal::NotPermitted),
        ),
        (
            Ended::Code {
                sck: false,
                code: -3801,
            },
            Failure::Fault(-3801),
        ),
        (
            Ended::Code {
                sck: true,
                code: -3802,
            },
            Failure::Fault(-3802),
        ),
        (
            Ended::Refused(Refusal::Excluded),
            Failure::Refused(Refusal::Excluded),
        ),
    ];
    for (ended, failure) in cases {
        let mut fake = Fake::new();
        fake.shoot = Shoot::Fail(ended);
        let fake = Arc::new(fake);
        assert_eq!(shot(&fake, LONG, LONG), Err(failure));
        assert_eq!(fake.released.load(Ordering::SeqCst), 0);
    }
}
