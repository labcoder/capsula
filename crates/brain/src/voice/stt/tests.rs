//! The owner's control flow against a fake engine whose decodes wait for the test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::{Decode, Decoder, Served, State, SttBusy, SttHandle, SttResult};
use crate::speech::{NativeError, SpeechError};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Loaded(PathBuf),
    /// A decode started, named by its sample count.
    Decoding(usize),
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
    release: mpsc::Sender<()>,
    fail_load: Arc<AtomicBool>,
    /// The running decode's worker is lost when it's released.
    lose: Arc<AtomicBool>,
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
        let exit = OnExit(events_in.clone());
        let handle = SttHandle::spawn_with(move |models: &Path, serve| {
            let _exit = &exit;
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
                return Err(SpeechError::WorkerLost);
            }
            events_in.send(Event::TornDown).expect("the test watches");
            Ok(())
        })
        .expect("the worker starts");
        Self {
            handle,
            events,
            release,
            fail_load,
            lose,
        }
    }

    fn loaded() -> Self {
        let harness = Self::new();
        harness.handle.load("models".into());
        assert_eq!(harness.event(), Event::Loaded("models".into()));
        harness.wait_ready();
        harness
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
    harness.handle.unload();
    assert_eq!(answer(waiting), SttResult::Stale);
    assert_eq!(
        answer(harness.decode(3).expect("answered")),
        SttResult::NotLoaded,
        "a decode after unload isn't held behind the running one"
    );
    harness.release();
    assert_eq!(answer(running), text(1));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(
        answer(harness.decode(4).expect("answered")),
        SttResult::NotLoaded
    );
}

#[test]
fn load_unload_load_during_a_decode_stays_loaded_and_loads_once() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.handle.load("models".into());
    harness.handle.unload();
    harness.handle.load("models".into());
    harness.release();
    assert_eq!(answer(running), text(1));
    let next = harness.decode(2).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(2));
    harness.release();
    assert_eq!(answer(next), text(2));
    harness.no_event();
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
    harness.handle.load("models".into());
    for _ in 0..500 {
        if harness.handle.control.lock().want.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        answer(harness.decode(1).expect("answered")),
        SttResult::NotLoaded
    );
    harness.fail_load.store(false, Ordering::SeqCst);
    harness.handle.load("models".into());
    assert_eq!(harness.event(), Event::Loaded("models".into()));
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
    harness.lose.store(false, Ordering::SeqCst);
    harness.handle.load("models".into());
    assert_eq!(harness.event(), Event::Loaded("models".into()));
}

#[test]
fn a_new_models_folder_reloads_and_serves_the_decode_left_waiting() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    let waiting = harness.decode(2).expect("waits");
    harness.handle.load("other".into());
    harness.release();
    assert_eq!(answer(running), text(1));
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.event(), Event::Loaded("other".into()));
    assert_eq!(harness.event(), Event::Decoding(2));
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
