//! The owner's control flow against a fake engine whose decodes wait for the test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{broadcast, oneshot};

use super::{Decode, Decoder, Served, State, SttBusy, SttHandle, SttResult};
use crate::speech::{NativeError, SpeechError};
use crate::supervise::Generation;
use crate::voice::{LoadEvent, LoadReport, Reports};

const WAIT: Duration = Duration::from_secs(5);

mod concurrent;

#[test]
fn admitted_use_is_current_ready_not_busy_and_refreshes_the_existing_idle_clock() {
    use kumochi_protocol::message::{
        HOLD_DRAIN_TIMEOUT_MS, HOLD_START_TIMEOUT_MS, HoldFailure, MAX_HOLD_CHUNKS,
    };
    let harness = Harness::loaded();
    harness.handle.control.update(|state| {
        state.idle_since = Instant::now()
            .checked_sub(Duration::from_secs(899))
            .unwrap();
    });
    let admitted_at = Instant::now();
    assert_eq!(harness.handle.admit_use(), Ok(1));
    assert!(harness.handle.control.lock().idle_since >= admitted_at);
    assert!(harness.handle.current_use(1));
    assert!(!harness.handle.current_use(2));
    let (reply, _receive) = oneshot::channel();
    harness.handle.control.update(|state| {
        state.decode_cut = Some(super::DecodeCut::Delivery);
        state.waiting = Some(Decode {
            rate: 16_000,
            samples: vec![1],
            reply,
        });
    });
    assert_eq!(harness.handle.admit_use(), Err(HoldFailure::Busy));
    let (reply, _receive) = oneshot::channel();
    assert_eq!(
        harness.handle.decode_for(
            1,
            Decode {
                rate: 16_000,
                samples: vec![2],
                reply
            }
        ),
        Err(HoldFailure::Busy)
    );
    harness.handle.unload(1);
    assert!(!harness.handle.current_use(1));
    assert_eq!(harness.handle.admit_use(), Err(HoldFailure::NotLoaded));
    let (reply, _receive) = oneshot::channel();
    assert_eq!(
        harness.handle.decode_for(
            1,
            Decode {
                rate: 16_000,
                samples: vec![2],
                reply
            }
        ),
        Err(HoldFailure::NotLoaded)
    );
    harness
        .handle
        .control
        .update(|state| state.decode_cut = None);
    let audio = Duration::from_millis(u64::from(MAX_HOLD_CHUNKS) * 20);
    let entire_hold = audio
        + Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS + HOLD_DRAIN_TIMEOUT_MS))
        + crate::speech::DEADLINES.decode
        + audio * crate::speech::DEADLINES.decode_per_mille / 1000;
    assert!(entire_hold < Duration::from_mins(15));
}

pub(crate) fn held_for_runtime(
    reports: Reports,
    load: mpsc::Receiver<()>,
) -> (SttHandle, impl Fn() -> Option<Duration>) {
    held_for_runtime_checked(reports, load, |rate, samples| {
        format!("{rate}:{}", samples.len())
    })
}

pub(crate) fn held_for_runtime_checked(
    reports: Reports,
    load: mpsc::Receiver<()>,
    check: impl Fn(u32, &[i16]) -> String + Send + Sync + 'static,
) -> (SttHandle, impl Fn() -> Option<Duration>) {
    let release = Arc::new(Mutex::new(load));
    let (_losses, losses) = broadcast::channel(4);
    let handle = SttHandle::spawn_with(reports, losses, move |_, serve| {
        release
            .lock()
            .expect("one engine")
            .recv_timeout(WAIT)
            .expect("release load");
        let served = serve(
            Generation(0),
            &RuntimeDecoder {
                release: Arc::clone(&release),
                check: &check,
            },
        );
        assert_eq!(served, Served::Released);
        Ok(())
    })
    .expect("owner starts");
    let control = Arc::clone(&handle.control);
    (handle, move || control.lock().unload_after)
}

struct RuntimeDecoder<'a> {
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    check: &'a (dyn Fn(u32, &[i16]) -> String + Sync),
}

impl Decoder for RuntimeDecoder<'_> {
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError> {
        let text = (self.check)(rate, samples);
        self.release
            .lock()
            .expect("one decoder")
            .recv_timeout(WAIT)
            .expect("release decode");
        Ok(text)
    }
}

#[test]
fn idle_release_and_a_fresh_installer_request_reload_without_fetching_or_stale_revival() {
    use crate::voice::installer::{Durable, Effect, Installer};
    use kumochi_protocol::message::{PartState, SttUnload};
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    for (policy, seconds) in [(SttUnload::After15Min, 900), (SttUnload::After1Hour, 3600)] {
        let (mut installer, effects) = Installer::start(
            [Durable::Installed, Durable::Absent],
            [true; 2].map(Ok),
            [true; 2],
            &[],
        );
        assert_eq!(
            effects,
            [Effect::Load {
                part: SpeechInput,
                ask: 1
            }]
        );
        let harness = Harness::loaded();
        assert_eq!(
            installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
            []
        );
        harness.handle.set_unload(policy);
        harness.handle.control.update(|state| {
            assert_eq!(state.unload_after, Some(Duration::from_secs(seconds)));
            state.idle_since = std::time::Instant::now()
                .checked_sub(Duration::from_secs(seconds))
                .expect("fixture clock supports the idle interval");
        });
        assert_eq!(harness.event(), Event::TornDown);
        let terminal = harness.reports(1);
        assert_eq!(terminal, [report(1, LoadEvent::Unloaded)]);
        assert_eq!(installer.reported(SpeechInput, terminal[0]), []);
        assert_eq!(installer.view().speech_input, PartState::Unloaded);
        assert_eq!(
            answer(harness.decode(1).expect("not busy")),
            SttResult::NotLoaded
        );
        assert_eq!(
            installer.install(SpeechInput),
            [Effect::Load {
                part: SpeechInput,
                ask: 3
            }]
        );
        harness.handle.load("models".into(), 3);
        assert_eq!(harness.event(), Event::Loaded("models".into()));
        harness.wait_ready();
        for event in [LoadEvent::Loaded, LoadEvent::Unloaded, LoadEvent::Lost] {
            assert_eq!(installer.reported(SpeechInput, report(1, event)), []);
            assert_eq!(installer.view().speech_input, PartState::Loading);
        }
        assert_eq!(installer.reported(SpeechInput, harness.reports(1)[0]), []);
        assert_eq!(installer.view().speech_input, PartState::Ready);
        let decoded = harness.decode(2).expect("not busy");
        assert_eq!(harness.event(), Event::Decoding(2));
        harness.release();
        assert_eq!(answer(decoded), text(2));
        harness.handle.unload(1);
        assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
        assert_eq!(harness.handle.control.lock().current, Some(3));
        harness.handle.unload(3);
        assert_eq!(harness.event(), Event::TornDown);
        assert_eq!(harness.reports(1), [report(3, LoadEvent::Unloaded)]);
    }
}

#[test]
fn an_expired_idle_clock_never_interrupts_running_or_queued_decodes() {
    use kumochi_protocol::message::SttUnload;

    let harness = Harness::loaded();
    harness.handle.set_unload(SttUnload::After15Min);
    let first = harness.decode(1).expect("not busy");
    assert_eq!(harness.event(), Event::Decoding(1));
    let second = harness.decode(2).expect("one waiting decode");
    harness.handle.control.update(|state| {
        state.idle_since = std::time::Instant::now()
            .checked_sub(Duration::from_hours(1))
            .expect("fixture clock supports one hour of history");
    });
    harness.no_event();
    harness.no_report();
    harness.release();
    assert_eq!(answer(first), text(1));
    assert_eq!(harness.event(), Event::Decoding(2));
    harness.no_event();
    harness.no_report();
    let idle_since = harness.handle.control.lock().idle_since;
    harness.release();
    assert_eq!(answer(second), text(2));
    harness.wait_for(
        |state| state.idle_since != idle_since,
        "the worker never reset the idle clock after the second decode",
    );
    assert_eq!(harness.handle.control.lock().current, Some(1));
    harness.handle.control.update(|state| {
        state.idle_since = std::time::Instant::now()
            .checked_sub(Duration::from_mins(15))
            .expect("fixture clock supports fifteen minutes of history");
    });
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
}

#[test]
fn keep_warm_and_a_longer_timeout_preserve_the_loaded_owner() {
    use kumochi_protocol::message::SttUnload;

    let harness = Harness::loaded();
    harness.handle.set_unload(SttUnload::Kept);
    harness.handle.control.update(|state| {
        assert_eq!(state.unload_after, None);
        state.idle_since = std::time::Instant::now()
            .checked_sub(Duration::from_hours(1))
            .expect("fixture clock supports one hour of history");
    });
    harness.no_event();
    harness.no_report();
    harness.handle.control.update(|state| {
        state.idle_since = std::time::Instant::now()
            .checked_sub(Duration::from_mins(15))
            .expect("fixture clock supports fifteen minutes of history");
    });
    harness.handle.set_unload(SttUnload::After1Hour);
    harness.no_event();
    harness.no_report();
    harness.handle.set_unload(SttUnload::After15Min);
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
}

#[test]
fn idle_policy_without_a_wanted_model_neither_loads_nor_accepts_a_decode() {
    use kumochi_protocol::message::SttUnload;

    let harness = Harness::new();
    harness.handle.set_unload(SttUnload::After15Min);
    harness.handle.control.update(|state| {
        state.idle_since = std::time::Instant::now()
            .checked_sub(Duration::from_hours(1))
            .expect("fixture clock supports one hour of history");
    });
    assert_eq!(
        answer(harness.decode(1).expect("not busy")),
        SttResult::NotLoaded
    );
    harness.no_event();
    harness.no_report();
    assert_eq!(harness.handle.control.lock().current, None);
}

#[test]
fn the_condition_variable_expires_without_another_command() {
    let harness = Harness::loaded();
    harness.handle.control.update(|state| {
        state.unload_after = Some(Duration::from_millis(20));
        state.idle_since = std::time::Instant::now();
    });
    assert_eq!(harness.event(), Event::TornDown);
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Unloaded)]);
}

#[tokio::test]
async fn an_idle_owner_keeps_its_ask_until_teardown_returns_before_loading_a_new_one() {
    let (events, observed) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Arc::new(Mutex::new(released));
    let (reports, mut heard) = Reports::new();
    let (_losses, losses) = broadcast::channel(4);
    let handle = SttHandle::spawn_with(reports, losses, move |models, serve| {
        events
            .send(Event::Loaded(models.to_owned()))
            .expect("watched");
        assert_eq!(
            serve(
                Generation(0),
                &FakeDecoder {
                    events: events.clone(),
                    release: Arc::clone(&released),
                    lose: Arc::new(AtomicBool::new(false)),
                    gates: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
                    panic_at: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
                }
            ),
            Served::Released
        );
        events.send(Event::TornDown).expect("watched");
        released
            .lock()
            .expect("one engine")
            .recv_timeout(WAIT)
            .expect("release teardown");
        Ok(())
    })
    .expect("owner starts");
    handle.load("old-models".into(), 1);
    assert_eq!(
        observed.recv_timeout(WAIT).expect("load"),
        Event::Loaded("old-models".into())
    );
    assert_eq!(
        tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("report"),
        Some(report(1, LoadEvent::Loaded))
    );
    handle.control.update(|state| {
        state.unload_after = Some(Duration::from_millis(20));
        state.idle_since = std::time::Instant::now();
    });
    assert_eq!(
        observed.recv_timeout(WAIT).expect("teardown begins"),
        Event::TornDown
    );
    assert_eq!(handle.control.lock().current, Some(1));
    assert_eq!(
        heard.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    handle.set_unload(kumochi_protocol::message::SttUnload::Kept);
    handle.load("new-models".into(), 2);
    handle.unload(1);
    assert_eq!(observed.recv_timeout(Duration::from_millis(100)).ok(), None);
    release.send(()).expect("finish old teardown");
    assert_eq!(
        tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("report"),
        Some(report(1, LoadEvent::Unloaded))
    );
    assert_eq!(
        observed.recv_timeout(WAIT).expect("reload"),
        Event::Loaded("new-models".into())
    );
    assert_eq!(
        tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("report"),
        Some(report(2, LoadEvent::Loaded))
    );
    handle.unload(2);
    assert_eq!(
        observed.recv_timeout(WAIT).expect("unload"),
        Event::TornDown
    );
    release.send(()).expect("finish new teardown");
    assert_eq!(
        tokio::time::timeout(WAIT, heard.recv())
            .await
            .expect("report"),
        Some(report(2, LoadEvent::Unloaded))
    );
}

#[tokio::test]
async fn a_pending_receipt_waits_through_late_dispatch_and_wrong_generation_exits() {
    use crate::supervise::{Dispatch, Exit};
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};

    let (dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Pending);
    let (exits, mut retired) = broadcast::channel(4);
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    let mut release = Box::pin(super::released(&mut receipt, &mut retired, &retirement));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(release.as_mut().poll(&mut context), Poll::Pending);
    dispatch.send_replace(Dispatch::Dispatched(Generation(7)));
    exits
        .send(Exit {
            generation: Generation(6),
            confirmed: true,
        })
        .expect("observer");
    exits
        .send(Exit {
            generation: Generation(8),
            confirmed: true,
        })
        .expect("observer");
    assert_eq!(release.as_mut().poll(&mut context), Poll::Pending);
    exits
        .send(Exit {
            generation: Generation(7),
            confirmed: true,
        })
        .expect("observer");
    assert_eq!(release.await, Ok(()));
}

#[tokio::test]
async fn definitive_no_dispatch_releases_without_a_worker_exit() {
    use crate::supervise::Dispatch;
    let (dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Pending);
    let (_exits, mut retired) = broadcast::channel(4);
    dispatch.send_replace(Dispatch::NotDispatched);
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    assert_eq!(
        super::released(&mut receipt, &mut retired, &retirement).await,
        Ok(())
    );
}

#[tokio::test]
async fn an_unconfirmed_exit_does_not_release_a_dispatched_load() {
    use crate::supervise::{Dispatch, Exit};
    let (_dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Dispatched(Generation(7)));
    let (exits, mut retired) = broadcast::channel(4);
    exits
        .send(Exit {
            generation: Generation(7),
            confirmed: false,
        })
        .expect("observer");
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    assert_eq!(
        super::released(&mut receipt, &mut retired, &retirement).await,
        Err(SpeechError::ExitUnconfirmed)
    );
}

#[tokio::test]
async fn a_closed_pending_dispatch_receipt_does_not_prove_discard() {
    use crate::supervise::Dispatch;
    let (dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Pending);
    let (_exits, mut retired) = broadcast::channel(4);
    drop(dispatch);
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    assert_eq!(
        super::released(&mut receipt, &mut retired, &retirement).await,
        Err(SpeechError::ExitUnconfirmed)
    );
}

#[tokio::test]
async fn a_lagged_exit_subscription_does_not_release_a_dispatched_load() {
    use crate::supervise::{Dispatch, Exit};
    let (_dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Dispatched(Generation(7)));
    let (exits, mut retired) = broadcast::channel(1);
    exits
        .send(Exit {
            generation: Generation(7),
            confirmed: true,
        })
        .expect("observer");
    exits
        .send(Exit {
            generation: Generation(8),
            confirmed: true,
        })
        .expect("observer");
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    assert_eq!(
        super::released(&mut receipt, &mut retired, &retirement).await,
        Err(SpeechError::ExitUnconfirmed)
    );
}

#[tokio::test]
async fn a_closed_exit_subscription_does_not_release_a_dispatched_load() {
    use crate::supervise::Dispatch;
    let (_dispatch, mut receipt) = tokio::sync::watch::channel(Dispatch::Dispatched(Generation(7)));
    let (exits, mut closed) = broadcast::channel(1);
    drop(exits);
    let (retirement, _intents) = crate::supervise::fixture::retirement();
    assert_eq!(
        super::released(&mut receipt, &mut closed, &retirement).await,
        Err(SpeechError::ExitUnconfirmed)
    );
}

#[test]
fn an_unconfirmed_exit_keeps_its_physical_ask_and_cannot_acknowledge_unload() {
    use crate::voice::installer::{Durable, Effect, Folder, Installer, RemoveAnswer};
    use kumochi_protocol::message::PartState;
    use kumochi_protocol::voice::VoicePart::SpeechInput;

    let (mut installer, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true; 2].map(Ok),
        [true; 2],
        &[],
    );
    assert_eq!(
        effects,
        [Effect::Load {
            part: SpeechInput,
            ask: 1
        }]
    );
    let (reports, mut observed) = Reports::new();
    let (started, start) = mpsc::channel();
    let (release, leave) = mpsc::channel();
    let (exited, exit) = mpsc::channel();
    let guard = OnExit(exited);
    let (_losses, losses) = broadcast::channel(4);
    let handle = SttHandle::spawn_with(reports, losses, move |_, _| {
        let _guard = &guard;
        started.send(Event::Lost).expect("load started");
        leave.recv_timeout(WAIT).expect("release load failure");
        Err(SpeechError::ExitUnconfirmed)
    })
    .expect("owner starts");
    handle.load(PathBuf::from("models"), 1);
    assert_eq!(start.recv_timeout(WAIT).expect("load starts"), Event::Lost);
    release.send(()).expect("fail load");
    assert_eq!(exit.recv_timeout(WAIT).expect("owner ended"), Event::Exited);
    let fatal = observed
        .try_recv()
        .expect("fatal owner report precedes exit");
    assert_eq!(fatal, report(1, LoadEvent::Unavailable));
    assert_eq!(installer.reported(SpeechInput, fatal), []);
    assert_eq!(installer.view().speech_input, PartState::Unavailable);
    assert!(!installer.ready(SpeechInput));
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(installer.reload_input(), []);
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Refused, vec![])
    );
    handle.load(PathBuf::from("newer-models"), 2);
    handle.unload(1);
    handle.unload(2);
    assert_eq!(handle.control.lock().current, Some(1));
    assert_eq!(
        handle.control.lock().want.as_ref().map(|(_, ask)| *ask),
        Some(1)
    );
    assert!(matches!(
        observed.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

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
    gates: Arc<Mutex<std::collections::BTreeMap<usize, mpsc::Receiver<()>>>>,
    panic_at: Arc<std::sync::atomic::AtomicUsize>,
}

impl Decoder for FakeDecoder {
    #[expect(
        clippy::panic_in_result_fn,
        reason = "injects a decoder panic to verify worker-loss containment"
    )]
    fn decode(&self, rate: u32, samples: &[i16]) -> Result<String, SpeechError> {
        let gate = self
            .gates
            .lock()
            .expect("fixture gates")
            .remove(&samples.len());
        self.events
            .send(Event::Decoding(samples.len()))
            .expect("the test watches");
        if let Some(gate) = gate {
            gate.recv_timeout(WAIT).expect("release this decode");
        } else {
            self.release
                .lock()
                .expect("one decoder")
                .recv_timeout(WAIT)
                .expect("the test releases the decode");
        }
        assert_ne!(
            self.panic_at.load(Ordering::SeqCst),
            samples.len(),
            "decode fixture panic"
        );
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
    /// The next successful load waits for a release before it serves.
    hold_serve: Arc<AtomicBool>,
    losses: broadcast::Sender<Generation>,
    /// The live worker's generation, which a load reports.
    worker: Arc<AtomicU64>,
    /// The running decode's worker is lost when it's released.
    lose: Arc<AtomicBool>,
    gates: Arc<Mutex<std::collections::BTreeMap<usize, mpsc::Receiver<()>>>>,
    panic_at: Arc<std::sync::atomic::AtomicUsize>,
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
        let gates = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let decoding_gates = Arc::clone(&gates);
        let panic_at = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        let panicking = Arc::clone(&panic_at);
        let hold_load = Arc::new(AtomicBool::new(false));
        let holding = Arc::clone(&hold_load);
        let hold_serve = Arc::new(AtomicBool::new(false));
        let holding_serve = Arc::clone(&hold_serve);
        let (losses, _) = broadcast::channel(4);
        let worker = Arc::new(AtomicU64::new(0));
        let serving = Arc::clone(&worker);
        let exit = OnExit(events_in.clone());
        let (sender, reports) = Reports::new();
        let handle =
            SttHandle::spawn_with(sender, losses.subscribe(), move |models: &Path, serve| {
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
                let generation = Generation(serving.load(Ordering::SeqCst));
                events_in
                    .send(Event::Loaded(models.to_owned()))
                    .expect("the test watches");
                if holding_serve.swap(false, Ordering::SeqCst) {
                    released
                        .lock()
                        .expect("one engine")
                        .recv_timeout(WAIT)
                        .expect("the test releases the serve");
                }
                let served = serve(
                    generation,
                    &FakeDecoder {
                        events: events_in.clone(),
                        release: Arc::clone(&released),
                        lose: Arc::clone(&losing),
                        gates: Arc::clone(&decoding_gates),
                        panic_at: Arc::clone(&panicking),
                    },
                );
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
            hold_serve,
            losses,
            worker,
            lose,
            gates,
            panic_at,
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

    /// The live worker dies and the next replaces it: the supervisor broadcasts the dead one's
    /// generation, and the wake task pokes the thread.
    fn lose_worker(&self) {
        let dead = Generation(self.worker.fetch_add(1, Ordering::SeqCst));
        self.losses.send(dead).expect("the thread subscribed");
        self.handle.control.update(|_| ());
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
    harness.lose_worker();
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

#[test]
fn the_wake_task_brings_an_idle_loss_to_the_thread_and_ends_with_the_supervisor() {
    let harness = Harness::loaded();
    let task = super::wake_on_losses(
        Arc::clone(&harness.handle.control),
        harness.losses.subscribe(),
    );
    let (ended, end) = mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a test runtime")
            .block_on(task);
        ended.send(()).expect("the test waits for the end");
    });
    harness.losses.send(Generation(0)).expect("subscribed");
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
    drop(harness);
    end.recv_timeout(WAIT)
        .expect("the wake task ends with the supervisor");
}

#[test]
fn a_loss_after_the_load_returns_but_before_it_serves_is_kept() {
    let harness = Harness::new();
    harness.hold_serve.store(true, Ordering::SeqCst);
    harness.handle.load("models".into(), 1);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    harness.lose_worker();
    harness.release();
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(
        harness.reports(2),
        [report(1, LoadEvent::Loaded), report(1, LoadEvent::Lost)]
    );
    assert_eq!(
        answer(harness.decode(1).expect("answered")),
        SttResult::NotLoaded
    );
}

#[test]
fn a_loss_from_before_a_load_is_not_charged_to_it() {
    let harness = Harness::new();
    harness.lose_worker();
    harness.handle.load("models".into(), 1);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    harness.wait_ready();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Loaded)]);
    harness.no_report();
    let next = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.release();
    assert_eq!(answer(next), text(1));
}

/// The supervisor answers a lost worker's requests before it broadcasts the loss, so the loss can
/// arrive after a replacement load is serving.
#[test]
fn an_old_workers_loss_that_arrives_after_a_replacement_load_is_not_charged_to_it() {
    let harness = Harness::loaded();
    let running = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.lose.store(true, Ordering::SeqCst);
    harness.worker.store(1, Ordering::SeqCst);
    harness.release();
    assert_eq!(answer(running), SttResult::Failed(SpeechError::WorkerLost));
    assert_eq!(harness.event(), Event::Lost);
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
    harness.lose.store(false, Ordering::SeqCst);
    harness.handle.load("models".into(), 2);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    harness.wait_ready();
    assert_eq!(harness.reports(1), [report(2, LoadEvent::Loaded)]);
    harness
        .losses
        .send(Generation(0))
        .expect("the thread subscribed");
    let next = harness.decode(2).expect("runs");
    assert_eq!(
        harness.event(),
        Event::Decoding(2),
        "the old loss, queued before this decode, was read and skipped"
    );
    harness.release();
    assert_eq!(answer(next), text(2));
    harness.handle.unload(2);
    assert_eq!(
        harness.event(),
        Event::TornDown,
        "the live model is unloaded"
    );
    assert_eq!(harness.reports(1), [report(2, LoadEvent::Unloaded)]);
}

/// More old losses than the subscription holds: the lag drops the oldest, and the rest are still
/// skipped as older than the load.
#[test]
fn a_lagged_loss_queue_still_skips_old_workers_losses() {
    let harness = Harness::new();
    for _ in 0..5 {
        harness.lose_worker();
    }
    harness.handle.load("models".into(), 1);
    assert_eq!(harness.event(), Event::Loaded("models".into()));
    harness.wait_ready();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Loaded)]);
    let next = harness.decode(1).expect("runs");
    assert_eq!(harness.event(), Event::Decoding(1));
    harness.release();
    assert_eq!(answer(next), text(1));
    harness.lose_worker();
    assert_eq!(
        harness.event(),
        Event::Lost,
        "the serving worker's own loss still counts"
    );
    harness.release();
    assert_eq!(harness.reports(1), [report(1, LoadEvent::Lost)]);
}

/// An overflow that keeps the served worker's own loss still reports it lost, and one that keeps
/// only older workers' losses doesn't.
#[test]
fn a_lagged_read_finds_the_served_workers_loss_and_skips_older_ones() {
    let (sender, mut kept_own) = broadcast::channel(2);
    let mut only_older = sender.subscribe();
    for dead in 0..4 {
        sender.send(Generation(dead)).expect("subscribed");
    }
    assert!(super::lost(&mut kept_own, Generation(3)));
    assert!(!super::lost(&mut only_older, Generation(4)));
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

#[tokio::test]
async fn model_paths_speech_owner_reports_the_path_reason_after_release() {
    let (reports, mut heard) = Reports::new();
    let (_sender, losses) = broadcast::channel(4);
    let handle = SttHandle::spawn_with(reports, losses, |_, _| {
        Err(SpeechError::Native(NativeError::ModelPath))
    })
    .expect("owner starts");
    handle.load("models".into(), 1);
    assert_eq!(
        tokio::time::timeout(WAIT, heard.recv()).await.unwrap(),
        Some(report(1, LoadEvent::ModelPath))
    );
    let state = handle.control.lock();
    assert_eq!(state.want, None);
    assert_eq!(state.current, None);
    assert!(!state.ready);
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
