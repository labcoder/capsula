//! Hands large values to one long-lived dropper thread, so their destructors leave the thread
//! that discards them.
//!
//! Call [`start`] once before intake. Until then, and on the dropper itself, values drop in place.
// shortcut: the channel has no byte bound and is only metered by [`pending`]; add a queued-byte
// throttle before v1 goes live if legal churn grows its backlog.

use std::{
    cell::Cell,
    fmt, io,
    ops::{Deref, DerefMut},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        mpsc::{self, SendError, Sender},
    },
    thread,
};

type Payload = (Box<dyn Send>, usize);

static DROPPER: OnceLock<Dropper> = OnceLock::new();

thread_local! {
    static DROPPING: Cell<bool> = const { Cell::new(false) };
}

/// One dropper thread's queue, with the liveness and counters that thread shares.
struct Dropper {
    sender: Sender<Payload>,
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    alive: AtomicBool,
    values: AtomicUsize,
    bytes: AtomicUsize,
    peak: AtomicUsize,
}

/// Marks its own dropper dead when that thread exits, including by unwinding.
struct Exit(Arc<State>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.alive.store(false, Relaxed);
    }
}

impl Dropper {
    fn spawn(builder: thread::Builder) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Payload>();
        let state = Arc::new(State {
            alive: AtomicBool::new(true),
            ..State::default()
        });
        let shared = Arc::clone(&state);
        builder.spawn(move || {
            let _exit = Exit(Arc::clone(&shared));
            DROPPING.with(|dropping| dropping.set(true));
            for (value, bytes) in receiver {
                drop(value);
                shared.bytes.fetch_sub(bytes, Relaxed);
                shared.values.fetch_sub(1, Relaxed);
            }
        })?;
        Ok(Self { sender, state })
    }

    fn running(&self) -> bool {
        self.state.alive.load(Relaxed)
    }

    /// Queues a value, or leaks it once the thread has died: that is a failure, not shutdown,
    /// so the caller never runs the destructor.
    fn send(&self, value: Box<dyn Send>, bytes: usize) {
        let state = &self.state;
        state.values.fetch_add(1, Relaxed);
        let queued = state.bytes.fetch_add(bytes, Relaxed) + bytes;
        state.peak.fetch_max(queued, Relaxed);
        if let Err(SendError((value, bytes))) = self.sender.send((value, bytes)) {
            state.values.fetch_sub(1, Relaxed);
            state.bytes.fetch_sub(bytes, Relaxed);
            state.alive.store(false, Relaxed);
            std::mem::forget(value);
        }
    }

    fn pending(&self) -> Pending {
        Pending {
            values: self.state.values.load(Relaxed),
            bytes: self.state.bytes.load(Relaxed),
        }
    }

    fn take_peak(&self) -> usize {
        let state = &self.state;
        state.peak.swap(state.bytes.load(Relaxed), Relaxed)
    }
}

/// Starts the dropper; later calls only report whether it still runs.
/// # Errors
/// Returns the thread spawn failure, or an error once the dropper has died. Intake must not
/// start on either.
pub fn start() -> io::Result<()> {
    if DROPPER.get().is_none() {
        let dropper = Dropper::spawn(thread::Builder::new().name("scene-dropper".to_owned()))?;
        // A concurrent start may have won; dropping this sender ends the losing idle thread.
        drop(DROPPER.set(dropper));
    }
    if running() {
        Ok(())
    } else {
        Err(io::Error::other("the scene dropper has stopped"))
    }
}

/// Whether discarded values leave their callers: from a successful [`start`] until the dropper
/// dies, which only a panicking destructor in an unwinding build can cause. Stop intake when false.
pub fn running() -> bool {
    DROPPER.get().is_some_and(Dropper::running)
}

/// Values queued for or still in destruction, with the bytes their owners attributed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pending {
    /// Values sent and not yet destroyed.
    pub values: usize,
    /// Owner estimates, not a heap measurement; nested values are not counted again.
    pub bytes: usize,
}

/// Reads what the dropper has not finished destroying.
pub fn pending() -> Pending {
    DROPPER.get().map_or_default(Dropper::pending)
}

/// Returns the highest pending attributed bytes since the previous call.
pub fn take_peak() -> usize {
    DROPPER.get().map_or(0, Dropper::take_peak)
}

/// Owns a value whose destructor runs on the dropper thread, wherever this is dropped.
pub struct Deferred<T: Send + 'static> {
    value: Option<T>,
    bytes: usize,
}

impl<T: Send + 'static> Deferred<T> {
    /// Protects `value`, attributing `bytes` to it while it waits for destruction.
    pub fn new(value: T, bytes: usize) -> Self {
        Self {
            value: Some(value),
            bytes,
        }
    }

    /// Transforms the protected value; anything `f` discards drops on the calling thread.
    pub fn map<U: Send + 'static>(mut self, f: impl FnOnce(T) -> U) -> Deferred<U> {
        Deferred::new(f(self.take()), self.bytes)
    }

    /// Replaces the attribution, for an owner whose contents changed after protection began.
    pub fn attribute(&mut self, bytes: usize) {
        self.bytes = bytes;
    }

    /// The owner's cached byte estimate, not a measurement of the payload's heap.
    pub fn attribution(&self) -> usize {
        self.bytes
    }

    /// Releases the value to an owner that keeps it protected or off the UI thread.
    pub(crate) fn into_inner(mut self) -> T {
        self.take()
    }

    fn take(&mut self) -> T {
        self.value
            .take()
            .expect("a deferred value stays present until it is taken or dropped")
    }
}

impl<T: Send + 'static, E: Send + 'static> Deferred<Result<T, E>> {
    /// Splits a protected result without exposing either side.
    /// # Errors
    /// Returns the error, still protected, when the result was `Err`.
    pub fn transpose(mut self) -> Result<Deferred<T>, Deferred<E>> {
        let bytes = self.bytes;
        match self.take() {
            Ok(value) => Ok(Deferred::new(value, bytes)),
            Err(error) => Err(Deferred::new(error, bytes)),
        }
    }
}

impl<T: Send + 'static> Deref for Deferred<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
            .as_ref()
            .expect("a deferred value stays present until it is taken or dropped")
    }
}

impl<T: Send + 'static> DerefMut for Deferred<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("a deferred value stays present until it is taken or dropped")
    }
}

impl<T: Send + 'static> fmt::Debug for Deferred<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferred")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> Drop for Deferred<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        if !DROPPING.try_with(Cell::get).unwrap_or(true)
            && let Some(dropper) = DROPPER.get()
        {
            dropper.send(Box::new(value), self.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc::TryRecvError,
        time::{Duration, Instant},
    };

    struct Sentinel(Sender<thread::ThreadId>);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            self.0
                .send(thread::current().id())
                .expect("the test still listens");
        }
    }

    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("a destructor fails on the dropper");
        }
    }

    #[test]
    fn before_start_values_drop_in_place_and_intake_stays_closed() {
        assert!(
            !running(),
            "no test in this binary starts the global dropper"
        );
        assert_eq!((pending(), take_peak()), (Pending::default(), 0));
        let (ids, reports) = mpsc::channel();
        drop(Deferred::new(Sentinel(ids), 40));
        assert_eq!(reports.try_recv(), Ok(thread::current().id()));
    }

    #[test]
    fn a_dropper_that_cannot_spawn_reports_the_failure() {
        let oversized = thread::Builder::new().stack_size(usize::MAX / 2);
        assert!(
            Dropper::spawn(oversized).is_err(),
            "no platform reserves half the address space for one stack"
        );
    }

    #[test]
    fn a_dead_dropper_stops_running_and_leaks_later_values() {
        let dropper = Dropper::spawn(thread::Builder::new().name("dying-dropper".to_owned()))
            .expect("test dropper");
        assert!(dropper.running());
        dropper.send(Box::new(Panics), 7);
        let deadline = Instant::now() + Duration::from_secs(5);
        while dropper.running() {
            assert!(Instant::now() < deadline, "the panicking thread exits");
            thread::sleep(Duration::from_millis(1));
        }
        let pending = dropper.pending();
        let (ids, reports) = mpsc::channel();
        dropper.send(Box::new(Sentinel(ids)), 40);
        assert_eq!(
            reports.try_recv(),
            Err(TryRecvError::Empty),
            "the later value is neither destroyed nor released"
        );
        assert_eq!(
            dropper.pending(),
            pending,
            "the leaked value is not pending"
        );
    }
}
