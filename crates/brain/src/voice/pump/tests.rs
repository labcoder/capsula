use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use kumochi_protocol::message::{RunId, SegmentId};

use super::Pump;
use crate::voice::tts::{Engine, TtsError, TtsEvent, TtsHandle};

const WAIT: Duration = Duration::from_secs(5);

type Frame = Result<Vec<f32>, TtsError>;

/// Each stream yields what the test sends it, and ends when the test drops its sender.
struct Fake(Arc<Mutex<VecDeque<std_mpsc::Receiver<Frame>>>>);

impl Engine for Fake {
    type Stream = std_mpsc::IntoIter<Frame>;
    fn stream(&self, _text: &str) -> Result<Self::Stream, TtsError> {
        let next = self.0.lock().expect("unpoisoned").pop_front();
        next.map(IntoIterator::into_iter).ok_or(TtsError::Synthesis)
    }
}

struct Harness {
    pump: Pump,
    streams: Arc<Mutex<VecDeque<std_mpsc::Receiver<Frame>>>>,
    runtime: tokio::runtime::Runtime,
}

impl Harness {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a test runtime");
        let streams = Arc::new(Mutex::new(VecDeque::new()));
        let engine = Arc::clone(&streams);
        let tts = TtsHandle::spawn_with(runtime.handle().clone(), move |_dir: &Path| {
            Ok(Fake(Arc::clone(&engine)))
        })
        .expect("the owner thread starts");
        tts.load("models".into());
        Self {
            pump: Pump::new(tts),
            streams,
            runtime,
        }
    }

    /// Speaks `segment` of `run` at 24 kHz and returns the sender that feeds its stream.
    fn speak(&mut self, run: u64, segment: u32) -> std_mpsc::Sender<Frame> {
        let (frames, stream) = std_mpsc::channel();
        self.streams.lock().expect("unpoisoned").push_back(stream);
        self.pump
            .speak(RunId(run), SegmentId(segment), 24_000, "hello".into());
        frames
    }

    fn next(&mut self) -> (RunId, TtsEvent) {
        self.runtime
            .block_on(async { tokio::time::timeout(WAIT, self.pump.next()).await })
            .expect("an event within the wait")
    }

    /// Nothing comes within a moment, though the stream had time to fill the channel.
    fn assert_pending(&mut self) {
        let next = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(100), self.pump.next()).await
        });
        assert!(next.is_err(), "{next:?}");
    }
}

fn pcm(run: u64, segment: u32, samples: usize) -> (RunId, TtsEvent) {
    (
        RunId(run),
        TtsEvent::Pcm {
            segment,
            samples: vec![0; samples],
        },
    )
}

fn send(frames: &std_mpsc::Sender<Frame>, samples: usize) {
    frames
        .send(Ok(vec![0.0; samples]))
        .expect("the stream is alive");
}

#[test]
fn audio_is_read_only_on_credit_and_a_frame_may_overshoot_it() {
    let mut harness = Harness::new();
    let frames = harness.speak(1, 0);
    for _ in 0..3 {
        send(&frames, 2);
    }
    drop(frames);
    harness.assert_pending();
    harness.pump.credit(RunId(1), 3);
    assert_eq!(harness.next(), pcm(1, 0, 2));
    assert_eq!(harness.next(), pcm(1, 0, 2));
    harness.assert_pending();
    harness.pump.credit(RunId(1), 1);
    assert_eq!(harness.next(), pcm(1, 0, 2));
    harness.assert_pending();
    harness.pump.credit(RunId(1), 1);
    assert_eq!(
        harness.next(),
        (
            RunId(1),
            TtsEvent::Done {
                segment: 0,
                total: 6
            }
        )
    );
    harness.assert_pending();
}

#[test]
fn a_stream_that_closes_without_done_fails_its_segment() {
    let mut harness = Harness::new();
    let frames = harness.speak(1, 3);
    harness.pump.credit(RunId(1), 100);
    send(&frames, 2);
    assert_eq!(harness.next(), pcm(1, 3, 2));
    // A cancel the pump didn't make, as an unload does.
    harness.pump.tts.cancel();
    send(&frames, 2);
    assert_eq!(
        harness.next(),
        (
            RunId(1),
            TtsEvent::Failed {
                segment: 3,
                error: TtsError::Synthesis
            }
        )
    );
    harness.assert_pending();
}

#[test]
fn cancel_keeps_the_runs_credit_and_revoke_ends_it() {
    let mut harness = Harness::new();
    let first = harness.speak(1, 0);
    harness.pump.credit(RunId(1), 10);
    harness.pump.cancel(RunId(1));
    send(&first, 4);
    harness.assert_pending();

    let second = harness.speak(1, 1);
    send(&second, 4);
    assert_eq!(harness.next(), pcm(1, 1, 4), "no new credit needed");
    harness.pump.revoke(RunId(1));
    send(&second, 4);
    harness.assert_pending();
    harness
        .pump
        .speak(RunId(1), SegmentId(2), 24_000, "x".into());
    harness.assert_pending();
}

#[test]
fn another_run_starts_over_without_credit() {
    let mut harness = Harness::new();
    let old = harness.speak(1, 0);
    harness.pump.credit(RunId(1), 10);
    harness.pump.cancel(RunId(2));
    harness.pump.revoke(RunId(2));
    send(&old, 2);
    assert_eq!(
        harness.next(),
        pcm(1, 0, 2),
        "another run's ending touches nothing"
    );

    let new = harness.speak(2, 0);
    send(&old, 2);
    send(&new, 3);
    harness.assert_pending();
    harness.pump.credit(RunId(2), 1);
    assert_eq!(harness.next(), pcm(2, 0, 3));
}
