//! Added for Kumochi: which windows want a frame, kept apart from Win32 so it can be tested
//! (docs/development/gpui-patch.md).

use std::{
    cell::Cell,
    sync::{
        Condvar, LazyLock, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// How long GPUI keeps presenting after high-rate input (its `InputRateTracker`), in ms.
const INPUT_SUSTAIN_MS: u64 = 1000;

/// How often the vsync thread checks for a lost GPU device while no window wants a frame.
pub(crate) const IDLE_DEVICE_CHECK: Duration = Duration::from_secs(1);

/// How long a touchpad contact keeps its window updating DirectManipulation while it waits in
/// READY, in ms. A deliberate pan or pinch moves past the touchpad's recognition threshold within a
/// few hundred ms of contact, and GPUI already keeps presenting for one second after input
/// (`INPUT_SUSTAIN_MS`), so this covers recognition without waking longer than input already does.
// shortcut: unverified on real touchpad hardware (Rez has none). Whether READY needs `Update` to
// reach RUNNING, and how long fingers rest before moving, are OSC-132's desk checks.
const TOUCHPAD_CONTACT_MS: u64 = 1000;

/// Why `FrameDemand::wait` returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A window wants a frame.
    Frame,
    /// Nothing was asked for within the idle interval.
    Idle,
    /// The vsync thread is stopping.
    Stop,
}

/// Wakes the vsync thread only while some window wants a frame, so a settled app sleeps.
pub(crate) static FRAME_DEMAND: FrameDemand = FrameDemand::new();

pub(crate) struct FrameDemand {
    pending: Mutex<bool>,
    changed: Condvar,
}

impl FrameDemand {
    const fn new() -> Self {
        Self {
            pending: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn request(&self) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    /// Blocks until a request arrives and consumes it, `idle` passes, or `stop` is set. A
    /// stopping thread leaves any request for the thread that replaces it.
    pub(crate) fn wait(&self, stop: &AtomicBool, idle: Duration) -> Wake {
        let deadline = Instant::now() + idle;
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if stop.load(Ordering::Acquire) {
                return Wake::Stop;
            }
            if *pending {
                *pending = false;
                return Wake::Frame;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Wake::Idle;
            }
            pending = self
                .changed
                .wait_timeout(pending, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Wakes every waiting thread without adding demand, so a stopped one exits.
    pub(crate) fn wake_all(&self) {
        // Taking the lock orders this after a waiter's stop check, so the wakeup can't be lost.
        drop(self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.notify_all();
    }
}

static CLOCK_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds on a clock both the UI thread and the vsync thread can compare through an atomic.
pub(crate) fn clock_ms() -> u64 {
    u64::try_from(CLOCK_START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// What the vsync thread does with one window at a vblank.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FramePoll {
    /// Invalidate the window now.
    pub invalidate: bool,
    /// Input or a touchpad contact is still sustaining it, so keep the vsync thread awake.
    pub sustained: bool,
}

/// One window's frame demand, shared by its UI thread and the vsync thread.
pub(crate) struct FrameRequest {
    demand: &'static FrameDemand,
    requested: AtomicBool,
    /// Until when, on `clock_ms`, input keeps this window's frames coming.
    input_sustain_until: AtomicU64,
    /// Until when, on `clock_ms`, a touchpad contact waiting in READY keeps them coming.
    contact_until: AtomicU64,
}

impl Default for FrameRequest {
    fn default() -> Self {
        Self::new(&FRAME_DEMAND)
    }
}

impl FrameRequest {
    fn new(demand: &'static FrameDemand) -> Self {
        Self {
            demand,
            requested: AtomicBool::new(false),
            input_sustain_until: AtomicU64::new(0),
            contact_until: AtomicU64::new(0),
        }
    }

    /// Asks the vsync thread to invalidate this window at the next vblank.
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.demand.request();
    }

    /// GPUI keeps presenting through and after high-rate input without asking for frames, so
    /// input keeps them coming for as long as GPUI might present.
    pub(crate) fn sustain_after_input(&self) {
        self.sustain_after_input_at(clock_ms());
    }

    fn sustain_after_input_at(&self, now_ms: u64) {
        let until = now_ms.saturating_add(INPUT_SUSTAIN_MS);
        self.input_sustain_until.store(until, Ordering::Relaxed);
        self.request();
    }

    /// DirectManipulation reports status changes without being updated, so each one asks for a
    /// frame: a gesture that starts after its hit-test frame still gets frames to advance it.
    /// Leaving READY also ends a touchpad contact's keep-alive: a recognized gesture keeps its
    /// own frames through `gesture_updated`.
    pub(crate) fn gesture_status_changed(&self, ready: bool) {
        if !ready {
            self.contact_until.store(0, Ordering::Relaxed);
        }
        self.request();
    }

    /// DirectManipulation may need `Update` to move a touchpad contact from READY to a gesture,
    /// so the window keeps drawing, which updates it and repaints only a changed view, until the
    /// status leaves READY or `TOUCHPAD_CONTACT_MS` passes.
    pub(crate) fn touchpad_contact(&self) {
        self.touchpad_contact_at(clock_ms());
    }

    fn touchpad_contact_at(&self, now_ms: u64) {
        let until = now_ms.saturating_add(TOUCHPAD_CONTACT_MS);
        self.contact_until.store(until, Ordering::Relaxed);
        self.request();
    }

    /// Manual-update DirectManipulation only advances a gesture and its inertia when drawn.
    pub(crate) fn gesture_updated(&self, active: bool) {
        if active {
            self.request();
        }
    }

    /// Takes any pending request.
    pub(crate) fn poll(&self, now_ms: u64) -> FramePoll {
        let sustained = self.input_sustain_until.load(Ordering::Relaxed) > now_ms
            || self.contact_until.load(Ordering::Relaxed) > now_ms;
        let requested = self.requested.swap(false, Ordering::AcqRel);
        FramePoll {
            invalidate: requested || sustained,
            sustained,
        }
    }
}

/// A forced render the window owes GPUI until GPUI tries one. Device-lost recovery needs one, and
/// GPUI defers a forced render that arrives during another draw, or too soon for an inactive
/// window, without dirtying the window, so nothing else would ask for it again.
#[derive(Default)]
pub(crate) struct ForcedFrame {
    pending: Cell<bool>,
    serving: Cell<bool>,
}

impl ForcedFrame {
    pub(crate) fn latch(&self) {
        self.pending.set(true);
    }

    /// Starts asking GPUI for a frame, and returns whether it must be forced.
    pub(crate) fn begin(&self, force: bool) -> bool {
        let force = force || self.pending.get();
        self.pending.set(force);
        self.serving.set(force);
        force
    }

    /// GPUI tried to draw, successfully or not; inside a forced request, that serves it.
    pub(crate) fn drawn(&self) {
        if self.serving.get() {
            self.pending.set(false);
        }
    }

    /// Ends the request, and returns whether a forced render is still owed.
    pub(crate) fn end(&self) -> bool {
        self.serving.set(false);
        self.pending.get()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    use super::{ForcedFrame, FrameDemand, FramePoll, FrameRequest, Wake};

    /// Long enough that a test never reaches it: the waits below end on demand or stop.
    const NEVER: Duration = Duration::from_secs(3600);

    fn demand() -> &'static FrameDemand {
        Box::leak(Box::new(FrameDemand::new()))
    }

    fn poll(invalidate: bool, sustained: bool) -> FramePoll {
        FramePoll {
            invalidate,
            sustained,
        }
    }

    #[test]
    fn a_request_invalidates_once() {
        let request = FrameRequest::new(demand());
        assert_eq!(request.poll(0), poll(false, false));
        request.request();
        request.request();
        assert_eq!(request.poll(0), poll(true, false));
        assert_eq!(request.poll(0), poll(false, false));
    }

    #[test]
    fn input_sustains_for_exactly_one_second() {
        let request = FrameRequest::new(demand());
        request.sustain_after_input_at(1000);
        assert_eq!(request.poll(1000), poll(true, true));
        assert_eq!(request.poll(1999), poll(true, true));
        assert_eq!(request.poll(2000), poll(false, false));
    }

    #[test]
    fn later_input_extends_the_sustain() {
        let request = FrameRequest::new(demand());
        request.sustain_after_input_at(1000);
        request.sustain_after_input_at(1500);
        assert_eq!(request.poll(2499), poll(true, true));
        assert_eq!(request.poll(2500), poll(false, false));
    }

    #[test]
    fn a_request_wakes_the_vsync_thread_once() {
        let demand = demand();
        let running = AtomicBool::new(false);
        FrameRequest::new(demand).request();
        assert_eq!(demand.wait(&running, Duration::ZERO), Wake::Frame);
        assert_eq!(demand.wait(&running, Duration::ZERO), Wake::Idle);
        assert_eq!(demand.wait(&AtomicBool::new(true), Duration::ZERO), Wake::Stop);
    }

    #[test]
    fn a_stopped_thread_leaves_demand_for_its_replacement() {
        let demand = demand();
        demand.request();
        assert_eq!(demand.wait(&AtomicBool::new(true), NEVER), Wake::Stop);
        assert_eq!(demand.wait(&AtomicBool::new(false), NEVER), Wake::Frame);
    }

    #[test]
    fn a_blocked_thread_wakes_on_request() {
        let demand = demand();
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            let waiter = scope.spawn(|| demand.wait(&stop, NEVER));
            demand.request();
            assert_eq!(waiter.join().expect("the waiter doesn't panic"), Wake::Frame);
        });
    }

    #[test]
    fn a_blocked_thread_exits_on_stop() {
        let demand = demand();
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            let waiter = scope.spawn(|| demand.wait(&stop, NEVER));
            stop.store(true, Ordering::Release);
            demand.wake_all();
            assert_eq!(waiter.join().expect("the waiter doesn't panic"), Wake::Stop);
        });
    }

    #[test]
    fn a_gesture_that_starts_after_its_hit_test_frame_keeps_getting_frames() {
        let request = FrameRequest::new(demand());
        request.request();
        assert_eq!(request.poll(0), poll(true, false));
        request.gesture_updated(false);
        assert_eq!(request.poll(0), poll(false, false));
        request.gesture_status_changed(false);
        assert_eq!(request.poll(0), poll(true, false));
        for _ in 0..3 {
            request.gesture_updated(true);
            assert_eq!(request.poll(0), poll(true, false));
        }
        request.gesture_status_changed(true);
        assert_eq!(request.poll(0), poll(true, false));
        request.gesture_updated(false);
        assert_eq!(request.poll(0), poll(false, false));
    }

    #[test]
    fn a_new_hit_test_rearms_the_keep_alive() {
        let request = FrameRequest::new(demand());
        request.touchpad_contact_at(1000);
        assert_eq!(request.poll(1000), poll(true, true));
        request.touchpad_contact_at(1400);
        assert_eq!(request.poll(2399), poll(true, true));
        assert_eq!(request.poll(2400), poll(false, false));
    }

    #[test]
    fn leaving_ready_ends_the_keep_alive() {
        let request = FrameRequest::new(demand());
        request.touchpad_contact_at(1000);
        request.gesture_status_changed(true);
        assert_eq!(request.poll(1001), poll(true, true));
        request.gesture_status_changed(false);
        assert_eq!(request.poll(1002), poll(true, false));
        assert_eq!(request.poll(1003), poll(false, false));
    }

    #[test]
    fn nothing_runs_at_rest_once_the_keep_alive_expires() {
        let demand = demand();
        let request = FrameRequest::new(demand);
        let running = AtomicBool::new(false);
        request.touchpad_contact_at(1000);
        assert_eq!(demand.wait(&running, Duration::ZERO), Wake::Frame);
        assert_eq!(request.poll(1000), poll(true, true));
        assert_eq!(request.poll(1999), poll(true, true));
        for now in [2000, 2001, 62_000] {
            assert_eq!(request.poll(now), poll(false, false));
        }
        assert_eq!(demand.wait(&running, Duration::ZERO), Wake::Idle);
    }

    #[test]
    fn a_forced_render_survives_repeated_deferral() {
        let forced = ForcedFrame::default();
        forced.latch();
        for _ in 0..3 {
            assert!(forced.begin(false));
            assert!(forced.end());
        }
        assert!(forced.begin(false));
        forced.drawn();
        assert!(!forced.end());
        assert!(!forced.begin(false));
        forced.drawn();
        assert!(!forced.end());
    }

    #[test]
    fn a_requested_force_is_owed_until_drawn() {
        let forced = ForcedFrame::default();
        assert!(forced.begin(true));
        assert!(forced.end());
        assert!(forced.begin(false));
        forced.drawn();
        assert!(!forced.end());
    }

    #[test]
    fn a_draw_outside_a_forced_request_does_not_serve_it() {
        let forced = ForcedFrame::default();
        forced.latch();
        forced.drawn();
        assert!(forced.begin(false));
        forced.drawn();
        assert!(!forced.end());
    }
}
