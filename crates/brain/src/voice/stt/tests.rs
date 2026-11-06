//! The owner's control flow against a fake engine whose decodes wait for the test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{broadcast, oneshot};

use super::{Control, Decode, Decoder, Served, State, SttBusy, SttHandle, SttResult};
use crate::speech::{NativeError, SpeechError};
use crate::supervise::WorkerLost;
use crate::voice::{LoadEvent, LoadReport, Reports};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Loaded(PathBuf),
    /// A decode started, named by its sample count.
    Decoding(usize),
    /// Serving ended in a loss; the engine waits for a release before it reports the error.
    Lost,
    TornDown,
    Exited,
}

struct FakeDecoder {
    events: mpsc::Sender<Event>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    lose: Arc<AtomicBool>,
}

impl Decoder for FakeDecoder {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError> {
        self.events
            .send(Event::Decoding(samples.len()))
            .expect("the test watches");
        self.release
            .lock()
            .expect("one decoder")
            .recv_timeout(WAIT)
            .expect("the test releases the decode");
        if self.lose.load(Ordering::SeqCst) {
            return Err(SpeechError::WorkerLost);
        }
        Ok(format!("{rate}:{}", samples.len()))
    }
}

/// Sends `Exited` when the worker drops its engine, which it does on the way out.
struct OnExit(mpsc::Sender<Event>);

impl Drop for OnExit {
    fn drop(&mut self) {
        self.0.send(Event::Exited).ok();
    }
}

struct Harness {
    handle: SttHandle,
    events: mpsc::Receiver<Event>,
    reports: Mutex<UnboundedReceiver<LoadReport>>,
    release: mpsc::Sender<()>,
    fail_load: Arc<AtomicBool>,
    /// The next load waits for a release before it finishes.
    hold_load: Arc<AtomicBool>,
    /// The running decode's worker is lost when it's released.
    lose: Arc<AtomicBool>,
}

fn report(ask: u64, event: LoadEvent) -> LoadReport {
    LoadReport { ask, event }
}

impl Harness {
    fn new() -> Self {
        let (events_in, events) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Arc::new(Mutex::new(released));
        let fail_load = Arc::new(AtomicBool::new(false));
        let fail = Arc::clone(&fail_load);
        let lose = Arc::new(AtomicBool::new(false));
        let losing = Arc::clone(&lose);
        let hold_load = Arc::new(AtomicBool::new(false));
        let holding = Arc::clone(&hold_load);
        let exit = OnExit(events_in.clone());
        let (sender, reports) = Reports::new();
        let handle = SttHandle::spawn_with(sender, move |models: &Path, serve| {
            let _exit = &exit;
            if holding.swap(false, Ordering::SeqCst) {
                released
                    .lock()
                    .expect("one engine")
                    .recv_timeout(WAIT)
                    .expect("the test releases the load");
            }
            if fail.load(Ordering::SeqCst) {
                return Err(SpeechError::Native(NativeError::Load));
            }
            events_in
                .send(Event::Loaded(models.to_owned()))
                .expect("the test watches");
            let served = serve(&FakeDecoder {
                events: events_in.clone(),
                release: Arc::clone(&released),
                lose: Arc::clone(&losing),
            });
            if served == Served::Lost {
                events_in.send(Event::Lost).expect("the test watches");
                released
                    .lock()
                    .expect("one engine")
                    .recv_timeout(WAIT)
                    .expect("the test releases the loss");
                return Err(SpeechError::WorkerLost);
            }
            events_in.send(Event::TornDown).expect("the test watches");
            Ok(())
        })
        .expect("the worker starts");
        Self {
            handle,
            events,
            reports: Mutex::new(reports),
            release,
            fail_load,
            hold_load,
            lose,
        }
    }

    /// Loaded for ask 1.
    fn loaded() -> Self {
        let harness = Self::new();
        harness.handle.load("models".into(), 1);
        assert_eq!(harness.event(), Event::Loaded("models".into()));
        harness.wait_ready();
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Loaded)]);
        harness
    }

    /// The next `count` reports, in order.
    fn reports(&self, count: usize) -> Vec<LoadReport> {
        let mut reports = self.reports.lock().expect("one reader");
        let mut got = Vec::new();
        for _ in 0..500 {
            while let Ok(next) = reports.try_recv() {
                got.push(next);
            }
            if got.len() >= count {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.len(), count, "reports: {got:?}");
        got
    }

    fn no_report(&self) {
        std::thread::sleep(Duration::from_millis(100));
        let next = self.reports.lock().expect("one reader").try_recv().ok();
        assert_eq!(next, None);
    }

    fn event(&self) -> Event {
        self.events.recv_timeout(WAIT).expect("an event")
    }

    fn no_event(&self) {
        assert_eq!(
            self.events.recv_timeout(Duration::from_millis(100)).ok(),
            None
        );
    }

    fn wait_ready(&self) {
        self.wait_for(|state| state.ready, "the model never became ready");
    }

    fn wait_for(&self, done: impl Fn(&State) -> bool, never: &str) {
        for _ in 0..500 {
            if done(&self.handle.control.lock()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{never}");
    }

    /// Asks for a decode of `len` samples; the sample count names it in events and replies.
    fn decode(&self, len: usize) -> Result<oneshot::Receiver<SttResult>, SttBusy> {
        let (reply, answer) = oneshot::channel();
        self.handle
            .try_decode(Decode {
                rate: 16_000,
                samples: vec![0; len],
                reply,
            })
            .map(|()| answer)
    }

    fn release(&self) {
        self.release.send(()).expect("a decoder waits");
    }
}

fn answer(reply: oneshot::Receiver<SttResult>) -> SttResult {
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || done.send(reply.blocking_recv()).ok());
    wait.recv_timeout(WAIT)
        .expect("an answer in time")
        .expect("the worker answered")
}

fn text(len: usize) -> SttResult {
    SttResult::Text(format!("16000:{len}"))
}

#[test]
fn debug_shows_no_samples_and_no_transcript() {
    let marker = "kumochi-transcript-sentinel";
    let bytes = format!("{:?}", marker.as_bytes());
    let text = format!("{:?}", SttResult::Text(marker.into()));
    assert_eq!(text, format!("Text({} bytes)", marker.len()));
    assert!(!text.contains(marker) && !text.contains(&bytes[1..bytes.len() - 1]));

    let (reply, _answer) = oneshot::channel();
    let decode = format!(
        "{:?}",
        Decode {
            rate: 16_000,
            samples: vec![31_337; 3],
            reply,
        }
    );
    assert!(!decode.contains("31337"), "{decode}");
    assert!(decode.contains("samples: 3"), "{decode}");
}

#[test]
fn one_decode_runs_one_waits_and_a_third_is_busy() {
    let harness = Harness::loaded();
    let first = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let second = harness.decode(2).expect("waits");
    assert_eq!(harness.decode(3).err(), Some(SttBusy));
    harness.release();
    assert_eq!(answer(first), text(1));
    assert_eq!(harness.event(), Event::Decoding(2));
    harness.release();
    assert_eq!(answer(second), text(2));
}

#[test]
fn unload_answers_the_waiting_decode_at_once_and_tears_down_after_the_running_one() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let waiting = harness.decode(2).expect("waits");
    harness.handle.unload(1);
    assert_eq!(answer(waiting), SttResult::Stale);
    assert_eq!(
        answer(harness.decode(3).expect("answered")),
        SttResult::NotLoaded,
        "a decode after unload isn't held behind the running one"
    );
    harness.no_report();
    harness.release();
    assert_eq!(answer(running), text(1));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
    assert_eq!(
        answer(harness.decode(4).expect("answered")),
        SttResult::NotLoaded
    );
}

#[test]
fn a_newer_ask_during_a_decode_reloads_after_it() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.handle.load("models".into(), 2);
    harness.handle.unload(2);
    harness.handle.load("models".into(), 3);
    assert_eq!(
        harness.reports(1),
        [report(2, LoadEvent::Unloaded)],
        "ask 2 never reached the thread"
    );
    harness.release();
    assert_eq!(answer(running), text(1));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    assert_eq!(
        harness.reports(2),
        [report(1, LoadEvent::Unloaded), report(3, LoadEvent::Loaded)]
    );
    harness.wait_ready();
    let next = harness.decode(2).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(2));
    harness.release();
    assert_eq!(answer(next), text(2));
    harness.no_event();
}

#[test]
fn a_load_that_finishes_after_its_unload_is_discarded_and_the_newer_ask_loads() {
    let harness = Harness::new();
    harness.hold_load.store(true, Ordering::SeqCst);
    harness.handle.load("models".into(), 1);
    harness.wait_for(|state| state.current == Some(1), "the load never started");
    harness.handle.unload(1);
    harness.handle.load("models".into(), 2);
    harness.no_report();
    harness.release();
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    assert_eq!(
        harness.reports(2),
        [
            report(1, LoadEvent::Discarded),
            report(2, LoadEvent::Loaded)
        ],
        "the late load is freed without a Loaded, and nothing unloads the newer ask"
    );
    harness.wait_ready();
    let next = harness.decode(5).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(5));
    harness.release();
    assert_eq!(answer(next), text(5));
    harness.no_report();
}

#[test]
fn unloading_an_older_ask_leaves_the_served_model_alone() {
    let harness = Harness::loaded();
    harness.handle.unload(7);
    assert_eq!(harness.reports(1), [report(7, LoadEvent::Unloaded)]);
    harness.no_event();
    let next = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.release();
    assert_eq!(answer(next), text(1));
}

#[test]
fn an_idle_loss_ends_serving_and_is_reported_lost() {
    let harness = Harness::loaded();
    harness.handle.worker_lost();
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
    assert_eq!(
        answer(harness.decode(1).expect("answered")),
        SttResult::NotLoaded
    );
    harness.handle.load("models".into(), 2);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    assert_eq!(harness.reports(1), [report(2, LoadEvent::Loaded)]);
}

#[tokio::test]
async fn each_worker_loss_marks_the_model_lost_until_the_supervisor_goes() {
    let control = Arc::new(Control {
        state: std::sync::Mutex::new(State {
            want: None,
            current: None,
            lost: false,
            ready: false,
            waiting: None,
            shutdown: false,
        }),
        wake: std::sync::Condvar::new(),
    });
    let (losses, receiver) = broadcast::channel(1);
    let task = tokio::spawn(super::forward_losses(Arc::clone(&control), receiver));
    losses.send(WorkerLost).expect("subscribed");
    for _ in 0..500 {
        if control.lock().lost {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(control.lock().lost, "the loss reached the owner");
    drop(losses);
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the forwarder ends with the supervisor")
        .expect("no panic");
}

#[test]
fn a_decode_before_any_load_is_not_loaded() {
    let harness = Harness::new();
    assert_eq!(
        answer(harness.decode(1).expect("answered")),
        SttResult::NotLoaded
    );
    harness.no_event();
}

#[test]
fn a_decode_whose_caller_left_is_skipped() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    drop(harness.decode(2).expect("waits"));
    harness.release();
    assert_eq!(answer(running), text(1));
    harness.wait_for(
        |state| state.waiting.is_none(),
        "the worker never took the abandoned decode",
    );
    let next = harness.decode(3).expect("runs");
    assert_eq!(
        harness.event(),
        Event::Decoding(3),
        "the abandoned decode never ran"
    );
    harness.release();
    assert_eq!(answer(next), text(3));
}

#[test]
fn a_failed_load_leaves_it_unloaded_until_asked_again() {
    let harness = Harness::new();
    harness.fail_load.store(true, Ordering::SeqCst);
    harness.handle.load("models".into(), 1);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Failed)]);
    assert!(harness.handle.control.lock().want.is_none());
    assert_eq!(
        answer(harness.decode(1).expect("answered")),
        SttResult::NotLoaded
    );
    harness.fail_load.store(false, Ordering::SeqCst);
    harness.handle.load("models".into(), 2);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    assert_eq!(harness.reports(1), [report(2, LoadEvent::Loaded)]);
}

#[test]
fn a_lost_worker_fails_the_running_decode_and_leaves_it_unloaded_until_asked_again() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let waiting = harness.decode(2).expect("waits");
    harness.lose.store(true, Ordering::SeqCst);
    harness.release();
    assert_eq!(answer(running), SttResult::Failed(SpeechError::WorkerLost));
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(
        answer(waiting),
        SttResult::NotLoaded,
        "nothing runs on the lost model"
    );
    assert_eq!(
        answer(harness.decode(3).expect("answered")),
        SttResult::NotLoaded
    );
    harness.no_event();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
    harness.lose.store(false, Ordering::SeqCst);
    harness.handle.load("models".into(), 2);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
}

#[test]
fn a_load_asked_for_while_a_loss_is_cleaned_up_still_reaches_the_worker() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let waiting = harness.decode(2).expect("waits");
    harness.lose.store(true, Ordering::SeqCst);
    harness.release();
    assert_eq!(answer(running), SttResult::Failed(SpeechError::WorkerLost));
    assert_eq!(harness.event(), Event::Lost);
    harness.lose.store(false, Ordering::SeqCst);
    harness.handle.load("models".into(), 2);
    harness.release();
    assert_eq!(
        answer(waiting),
        SttResult::NotLoaded,
        "the lost model's waiter is settled, not carried over"
    );
    assert_eq!(
        harness.reports(2),
        [report(1, LoadEvent::Lost), report(2, LoadEvent::Loaded)]
    );
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    harness.wait_ready();
    let next = harness.decode(3).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(3));
    harness.release();
    assert_eq!(answer(next), text(3));
}

#[test]
fn a_new_models_folder_reloads_and_serves_the_decode_left_waiting() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let waiting = harness.decode(2).expect("waits");
    harness.handle.load("other".into(), 2);
    harness.release();
    assert_eq!(answer(running), text(1));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.event(), Event::Loaded("other".into()));
    assert_eq!(harness.event(), Event::Decoding(2));
    assert_eq!(
        harness.reports(2),
        [report(1, LoadEvent::Unloaded), report(2, LoadEvent::Loaded)]
    );
    harness.release();
    assert_eq!(answer(waiting), text(2));
}

#[test]
fn dropping_the_handle_tears_down_then_ends_the_thread() {
    let harness = Harness::loaded();
    drop(harness.handle);
    assert_eq!(
        harness.events.recv_timeout(WAIT).ok(),
        Some(Event::TornDown)
    );
    assert_eq!(harness.events.recv_timeout(WAIT).ok(), Some(Event::Exited));
}
