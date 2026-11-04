use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use kumochi_protocol::voice::{MODEL_FILES, VoicePart};
use tokio::sync::mpsc;
use tracing::Level;

use super::phonon::{Job, start_pool};
use super::{
    CONFIG, Engine, MAX_SAY_BYTES, Speak, TOKENIZER, TtsBusy, TtsError, TtsEvent, TtsHandle, VOICE,
    WEIGHTS, to_pcm,
};

const WAIT: Duration = Duration::from_secs(5);

type Frame = Result<Vec<f32>, TtsError>;

/// A scripted stream: the test hands it frames one at a time and sees when it's dropped.
struct FakeStream {
    frames: std_mpsc::Receiver<Frame>,
    dropped: std_mpsc::Sender<()>,
}

impl Iterator for FakeStream {
    type Item = Frame;
    fn next(&mut self) -> Option<Frame> {
        self.frames.recv().ok()
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.dropped
            .send(())
            .expect("the test outlives its streams");
    }
}

struct Fake {
    streams: Arc<Mutex<std_mpsc::Receiver<FakeStream>>>,
    live: Arc<AtomicUsize>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Engine for Fake {
    type Stream = FakeStream;
    fn stream(&self, _text: &str) -> Result<FakeStream, TtsError> {
        self.streams
            .lock()
            .expect("no test thread panicked holding it")
            .recv_timeout(WAIT)
            .map_err(|_| TtsError::Synthesis)
    }
}

struct Harness {
    handle: TtsHandle,
    streams: std_mpsc::Sender<FakeStream>,
    live: Arc<AtomicUsize>,
    loads: Arc<AtomicUsize>,
    runtime: tokio::runtime::Runtime,
}

/// What a test holds for one scripted stream.
struct Script {
    frames: std_mpsc::SyncSender<Frame>,
    dropped: std_mpsc::Receiver<()>,
}

impl Script {
    /// Returns once the owner has taken the frame, since the channel holds nothing.
    fn send(&self, frame: Frame) {
        self.frames.send(frame).expect("the stream is still alive");
    }

    fn assert_dropped(&self) {
        self.dropped
            .recv_timeout(WAIT)
            .expect("the owner dropped the stream");
    }
}

impl Harness {
    fn new(load: Result<(), TtsError>) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a test runtime");
        let (streams, queued) = std_mpsc::channel();
        let queued = Arc::new(Mutex::new(queued));
        let live = Arc::new(AtomicUsize::new(0));
        let loads = Arc::new(AtomicUsize::new(0));
        let (live_in, loads_in) = (Arc::clone(&live), Arc::clone(&loads));
        let handle = TtsHandle::spawn_with(runtime.handle().clone(), move |_dir: &Path| {
            loads_in.fetch_add(1, Ordering::SeqCst);
            load?;
            live_in.fetch_add(1, Ordering::SeqCst);
            Ok(Fake {
                streams: Arc::clone(&queued),
                live: Arc::clone(&live_in),
            })
        })
        .expect("the owner thread starts");
        Self {
            handle,
            streams,
            live,
            loads,
            runtime,
        }
    }

    fn loaded() -> Self {
        let harness = Self::new(Ok(()));
        harness.handle.load("models".into());
        harness.wait_live(1);
        harness
    }

    fn script(&self) -> Script {
        let (frames, frames_rx) = std_mpsc::sync_channel(0);
        let (dropped_tx, dropped) = std_mpsc::channel();
        self.streams
            .send(FakeStream {
                frames: frames_rx,
                dropped: dropped_tx,
            })
            .expect("the engine holds the queue");
        Script { frames, dropped }
    }

    fn speak(&self, text: &str, capacity: usize) -> mpsc::Receiver<TtsEvent> {
        let (out, events) = mpsc::channel(capacity);
        self.handle
            .try_speak(Speak {
                segment: 7,
                generation: self.handle.generation(),
                text: text.into(),
                out,
            })
            .expect("nothing else is waiting");
        events
    }

    /// The next event, or `None` once the owner has dropped the sender.
    fn next(&self, events: &mut mpsc::Receiver<TtsEvent>) -> Option<TtsEvent> {
        self.runtime
            .block_on(async { tokio::time::timeout(WAIT, events.recv()).await })
            .expect("an event or a closed channel within the wait")
    }

    fn wait_live(&self, want: usize) {
        for _ in 0..500 {
            if self.live.load(Ordering::SeqCst) == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(self.live.load(Ordering::SeqCst), want, "live models");
    }
}

fn pcm(samples: Vec<i16>) -> TtsEvent {
    TtsEvent::Pcm {
        segment: 7,
        samples,
    }
}

fn failed(error: TtsError) -> TtsEvent {
    TtsEvent::Failed { segment: 7, error }
}

#[test]
fn text_bounds() {
    let harness = Harness::loaded();
    for text in [String::new(), "a\0b".into(), "a".repeat(MAX_SAY_BYTES + 1)] {
        let mut events = harness.speak(&text, 4);
        assert_eq!(harness.next(&mut events), Some(failed(TtsError::Text)));
        assert_eq!(harness.next(&mut events), None);
    }
    let script = harness.script();
    let mut events = harness.speak(&"a".repeat(MAX_SAY_BYTES), 4);
    drop(script.frames);
    assert_eq!(
        harness.next(&mut events),
        Some(TtsEvent::Done {
            segment: 7,
            total: 0
        })
    );
}

#[test]
fn a_bump_between_frames_drops_the_stream_and_sends_nothing_more() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut events = harness.speak("hello", 8);
    script.send(Ok(vec![0.0; 2]));
    script.send(Ok(vec![0.0; 3]));
    assert_eq!(harness.next(&mut events), Some(pcm(vec![0; 2])));
    assert_eq!(harness.next(&mut events), Some(pcm(vec![0; 3])));
    harness.handle.cancel();
    script.send(Ok(vec![0.0; 4]));
    script.assert_dropped();
    assert_eq!(harness.next(&mut events), None, "no frame 3 and no Done");
}

#[test]
fn a_bump_releases_an_owner_waiting_on_a_full_channel() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut events = harness.speak("hello", 1);
    script.send(Ok(vec![0.0; 1]));
    script.send(Ok(vec![0.0; 2]));
    std::thread::sleep(Duration::from_millis(50));
    harness.handle.cancel();
    script.assert_dropped();
    assert_eq!(harness.next(&mut events), Some(pcm(vec![0; 1])));
    assert_eq!(harness.next(&mut events), None, "frame 2 is never sent");
}

#[test]
fn dropping_the_consumer_releases_the_owner() {
    let harness = Harness::loaded();
    let full = harness.script();
    let events = harness.speak("hello", 1);
    full.send(Ok(vec![0.0; 1]));
    full.send(Ok(vec![0.0; 1]));
    std::thread::sleep(Duration::from_millis(50));
    drop(events);
    full.assert_dropped();

    let between = harness.script();
    let mut events = harness.speak("hello", 8);
    between.send(Ok(vec![0.0; 1]));
    assert_eq!(harness.next(&mut events), Some(pcm(vec![0; 1])));
    drop(events);
    between.send(Ok(vec![0.0; 1]));
    between.assert_dropped();
}

#[test]
fn unload_while_full_rejects_the_waiting_speak_and_drops_the_model() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut running = harness.speak("first", 1);
    script.send(Ok(vec![0.0; 1]));
    script.send(Ok(vec![0.0; 1]));
    let mut waiting = harness.speak("second", 1);
    let (out, _third) = mpsc::channel(1);
    assert_eq!(
        harness.handle.try_speak(Speak {
            segment: 9,
            generation: harness.handle.generation(),
            text: "third".into(),
            out,
        }),
        Err(TtsBusy)
    );
    harness.handle.unload();
    assert_eq!(
        harness.next(&mut waiting),
        Some(failed(TtsError::NotLoaded))
    );
    assert_eq!(harness.next(&mut waiting), None);
    script.assert_dropped();
    harness.wait_live(0);
    assert_eq!(harness.next(&mut running), Some(pcm(vec![0; 1])));
    assert_eq!(harness.next(&mut running), None);
}

#[test]
fn a_non_finite_sample_fails_and_ends_the_segment() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut events = harness.speak("hello", 8);
    script.send(Ok(vec![0.0, f32::NAN]));
    assert_eq!(harness.next(&mut events), Some(failed(TtsError::NonFinite)));
    assert_eq!(harness.next(&mut events), None);
    script.assert_dropped();
}

#[test]
fn thirty_seconds_is_the_limit() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut events = harness.speak("hello", 8);
    script.send(Ok(vec![0.0; 720_000]));
    drop(script.frames);
    assert_eq!(harness.next(&mut events), Some(pcm(vec![0; 720_000])));
    assert_eq!(
        harness.next(&mut events),
        Some(TtsEvent::Done {
            segment: 7,
            total: 720_000
        })
    );

    let script = harness.script();
    let mut events = harness.speak("hello", 8);
    script.send(Ok(vec![0.0; 720_001]));
    assert_eq!(harness.next(&mut events), Some(failed(TtsError::TooLong)));
    assert_eq!(harness.next(&mut events), None);
}

#[test]
fn a_synthesis_error_is_reported_once() {
    let harness = Harness::loaded();
    let script = harness.script();
    let mut events = harness.speak("hello", 8);
    script.send(Err(TtsError::Synthesis));
    assert_eq!(harness.next(&mut events), Some(failed(TtsError::Synthesis)));
    assert_eq!(harness.next(&mut events), None);
}

#[test]
fn gain_clipping_and_conversion_are_exact() {
    assert_eq!(
        to_pcm(&[0.0, 0.1, 0.5, -0.25, 0.6, -0.6, 1.0, -1.0]),
        Some(vec![0, 6172, 30861, -15430, 32767, -32767, 32767, -32767])
    );
    assert_eq!(to_pcm(&[0.0, f32::INFINITY]), None);
    assert_eq!(to_pcm(&[]), Some(vec![]));
}

#[test]
fn nothing_speaks_before_a_successful_load() {
    let harness = Harness::new(Ok(()));
    let mut events = harness.speak("hello", 4);
    assert_eq!(harness.next(&mut events), Some(failed(TtsError::NotLoaded)));

    let failing = Harness::new(Err(TtsError::Load));
    failing.handle.load("models".into());
    for _ in 0..500 {
        if failing.loads.load(Ordering::SeqCst) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut events = failing.speak("hello", 4);
    assert_eq!(failing.next(&mut events), Some(failed(TtsError::NotLoaded)));
    assert_eq!(failing.loads.load(Ordering::SeqCst), 1, "no retry loop");
}

#[test]
fn unload_and_dropping_the_handle_drop_the_model() {
    let harness = Harness::loaded();
    harness.handle.unload();
    harness.wait_live(0);
    harness.handle.load("models".into());
    harness.wait_live(1);
    assert_eq!(harness.loads.load(Ordering::SeqCst), 2);
    let Harness { handle, live, .. } = harness;
    drop(handle);
    for _ in 0..500 {
        if live.load(Ordering::SeqCst) == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the owner kept the model after the handle was dropped");
}

#[test]
fn the_pool_must_be_eight_wide_with_every_worker() {
    let start = |spawned: usize, width: usize| {
        move |config: xn::threadpool::PoolConfig, spawn: &dyn Fn(Job)| {
            assert_eq!(config.workers, 7);
            assert_eq!(config.spin_budget, Some(5_000));
            for _ in 0..spawned {
                spawn(Box::new(|| {}));
            }
            width
        }
    };
    assert_eq!(start_pool(start(7, 8), || 8, |_job| Ok(())), Ok(()));
    assert_eq!(
        start_pool(start(7, 7), || 7, |_job| Ok(())),
        Err(TtsError::Load)
    );
    assert_eq!(
        start_pool(start(7, 8), || 4, |_job| Ok(())),
        Err(TtsError::Load)
    );
    let calls = AtomicUsize::new(0);
    let one_fails = |_job: Job| {
        if calls.fetch_add(1, Ordering::SeqCst) == 3 {
            Err(std::io::Error::other("no thread"))
        } else {
            Ok(())
        }
    };
    assert_eq!(
        start_pool(start(7, 8), || 8, one_fails),
        Err(TtsError::Load)
    );
}

#[test]
fn phonon_loads_only_manifest_files() {
    for name in [CONFIG, WEIGHTS, TOKENIZER, VOICE] {
        assert!(
            MODEL_FILES
                .iter()
                .any(|file| file.part == VoicePart::SpokenReplies && file.name == name),
            "{name}"
        );
    }
}

#[test]
fn dependency_logging_is_off_at_every_level() {
    let filter = crate::logging::filter(Level::TRACE);
    for target in ["ptts", "ptts::synth", "xn", "xn_gemm", "tokenizers"] {
        assert!(!filter.would_enable(target, &Level::ERROR), "{target}");
    }
    assert!(filter.would_enable("kumochi_brain", &Level::TRACE));
}

/// Rez only, under `measuring.lock` on a quiet machine. Loads Phonon from the dev models
/// folder that `cargo xtask voice-assets link` fills, plays nothing, and prints timings only.
#[test]
#[ignore = "Rez only: loads the real model; run under measuring.lock"]
#[expect(
    clippy::too_many_lines,
    reason = "one measurement script, read top to bottom"
)]
fn measure_phonon() {
    use std::time::Instant;

    // A release build's data folder isn't the dev one the xtask links into, so VOICE_MODELS can
    // point at it.
    let models = std::env::var_os("VOICE_MODELS").map_or_else(
        || {
            crate::platform::data_dir()
                .expect("LOCALAPPDATA is set")
                .join(kumochi_protocol::voice::MODELS_DIR)
                .join(VoicePart::SpokenReplies.dir())
        },
        PathBuf::from,
    );
    let texts = Path::new("C:/dev/capsula-ws/spikes/voice-spike/audio");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a test runtime");
    let handle = TtsHandle::spawn(runtime.handle().clone()).expect("the owner thread starts");
    let speak = |text: &str| {
        let (out, events) = mpsc::channel(256);
        handle
            .try_speak(Speak {
                segment: 0,
                generation: handle.generation(),
                text: text.into(),
                out,
            })
            .expect("one Speak at a time");
        events
    };
    let next = |events: &mut mpsc::Receiver<TtsEvent>| {
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(120), events.recv())
                .await
                .expect("an event within two minutes")
        })
    };
    let ms = |since: Instant| since.elapsed().as_secs_f64() * 1_000.0;
    let summary = |mut values: Vec<f64>| {
        values.sort_by(f64::total_cmp);
        let rank = |percent: usize| values[(values.len() * percent).div_ceil(100).max(1) - 1];
        format!(
            "median {:.3} p95 {:.3} max {:.3}",
            rank(50),
            rank(95),
            rank(100)
        )
    };

    let started = Instant::now();
    handle.load(models);
    let mut events = speak("Hi.");
    while let Some(event) = next(&mut events) {
        assert!(!matches!(event, TtsEvent::Failed { .. }), "{event:?}");
    }
    println!("load + prewarm + one short speak: {:.0} ms", ms(started));
    println!("xn pool width: {}", xn::threadpool::size());

    for name in ["sentence", "paragraph"] {
        let text = std::fs::read_to_string(texts.join(format!("{name}.txt"))).expect("S5's text");
        let text = text.trim();
        let (mut first, mut rtf) = (Vec::new(), Vec::new());
        for run in 0..11 {
            let started = Instant::now();
            let mut events = speak(text);
            let mut first_audio = None;
            let mut samples = 0u32;
            while let Some(event) = next(&mut events) {
                match event {
                    TtsEvent::Pcm { samples: pcm, .. } => {
                        first_audio.get_or_insert_with(|| ms(started));
                        samples += u32::try_from(pcm.len()).expect("a frame fits u32");
                    }
                    TtsEvent::Done { total, .. } => assert_eq!(total, samples),
                    TtsEvent::Failed { error, .. } => panic!("{error:?}"),
                }
            }
            if run > 0 {
                first.push(first_audio.expect("audio"));
                rtf.push(ms(started) / 1_000.0 / (f64::from(samples) / 24_000.0));
            }
        }
        println!(
            "{name}: first audio ms {}; rtf {}",
            summary(first),
            summary(rtf)
        );
    }

    let paragraph = std::fs::read_to_string(texts.join("paragraph.txt")).expect("S5's text");
    let (mut between, mut computing) = (Vec::new(), Vec::new());
    for trial in 0..20u64 {
        let mut events = speak(paragraph.trim());
        assert!(matches!(next(&mut events), Some(TtsEvent::Pcm { .. })));
        let mid = trial % 2 == 1;
        if mid {
            std::thread::sleep(Duration::from_millis(5 + trial * 7));
        }
        let bumped = Instant::now();
        handle.cancel();
        while next(&mut events).is_some() {}
        if mid {
            computing.push(ms(bumped));
        } else {
            between.push(ms(bumped));
        }
    }
    println!("bump to idle ms, right after a frame: {}", summary(between));
    println!(
        "bump to idle ms, during computation: {}",
        summary(computing)
    );
}
