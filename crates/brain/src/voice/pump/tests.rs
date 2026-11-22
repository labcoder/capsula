use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use kumochi_protocol::message::{RunId, SegmentId};

use super::{FIRST_PCM_LIMIT, FirstPcmDeadline, OutputMode, Pump};
use crate::voice::tts::{Engine, Speak, TtsError, TtsEvent, TtsHandle};

const WAIT: Duration = Duration::from_secs(5);

type Frame = Result<Vec<f32>, TtsError>;

type Streams = Arc<Mutex<VecDeque<std_mpsc::Receiver<Frame>>>>;

/// Each stream yields what the test sends it, and ends when the test drops its sender. Its first
/// read tells the test, so the test knows the owner is inside that stream, waiting for a frame.
struct Fake {
    streams: Streams,
    entered: std_mpsc::Sender<()>,
}

struct FakeStream {
    frames: std_mpsc::IntoIter<Frame>,
    entered: Option<std_mpsc::Sender<()>>,
}

impl Iterator for FakeStream {
    type Item = Frame;
    fn next(&mut self) -> Option<Frame> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).expect("the test is alive");
        }
        self.frames.next()
    }
}

impl Engine for Fake {
    type Stream = FakeStream;
    fn stream(&self, _text: &str) -> Result<Self::Stream, TtsError> {
        let next = self.streams.lock().expect("unpoisoned").pop_front();
        next.map(|frames| FakeStream {
            frames: frames.into_iter(),
            entered: Some(self.entered.clone()),
        })
        .ok_or(TtsError::Synthesis)
    }
}

struct Harness {
    pump: Pump,
    streams: Streams,
    entered: std_mpsc::Receiver<()>,
    runtime: tokio::runtime::Runtime,
}

impl Harness {
    fn new() -> Self {
        Self::for_output(OutputMode::Legacy)
    }

    fn for_output(output: OutputMode) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a test runtime");
        let streams = Streams::default();
        let (entry, entered) = std_mpsc::channel();
        let engine = Arc::clone(&streams);
        let (reports, _) = crate::voice::Reports::new();
        let tts = TtsHandle::spawn_with(runtime.handle().clone(), reports, move |_dir: &Path| {
            Ok(Fake {
                streams: Arc::clone(&engine),
                entered: entry.clone(),
            })
        })
        .expect("the owner thread starts");
        tts.load("models".into(), 1);
        Self {
            pump: {
                let mut pump = Pump::new(tts);
                pump.set_output(output);
                pump
            },
            streams,
            entered,
            runtime,
        }
    }

    /// Waits until the owner is reading the next stream.
    fn wait_entered(&self) {
        self.entered
            .recv_timeout(WAIT)
            .expect("the owner opens the stream");
    }

    /// Speaks `segment` of `run` at 24 kHz and returns the sender that feeds its stream.
    fn speak(&mut self, run: u64, segment: u32) -> std_mpsc::Sender<Frame> {
        let (frames, stream) = std_mpsc::channel();
        self.streams.lock().expect("unpoisoned").push_back(stream);
        let _runtime = self.runtime.enter();
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

    fn advance(&self, by: Duration) {
        self.runtime.block_on(tokio::time::advance(by));
    }

    fn deadline(&self) -> FirstPcmDeadline {
        self.pump
            .first_pcm_deadline()
            .expect("a dispatched Speak is timed")
    }

    fn expire(&mut self, due: FirstPcmDeadline) -> Option<(RunId, SegmentId)> {
        let _runtime = self.runtime.enter();
        self.pump.expire_first_pcm(due)
    }

    fn wait_buffered(&self) {
        let until = std::time::Instant::now() + WAIT;
        let events = &self
            .pump
            .active
            .as_ref()
            .expect("the Speak is active")
            .events;
        while events.is_empty() && !events.is_closed() {
            assert!(
                std::time::Instant::now() < until,
                "the owner sends or closes"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
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
    send_value(frames, samples, 0.0);
}

fn send_value(frames: &std_mpsc::Sender<Frame>, samples: usize, value: f32) {
    frames
        .send(Ok(vec![value; samples]))
        .expect("the stream is alive");
}

fn ended(run: u64, segment: u32, error: TtsError) -> (RunId, TtsEvent) {
    (RunId(run), TtsEvent::Failed { segment, error })
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
fn a_failure_or_a_closed_stream_after_the_last_credit_needs_no_more() {
    let mut harness = Harness::new();
    let frames = harness.speak(1, 0);
    harness.pump.credit(RunId(1), 2);
    send(&frames, 2);
    frames
        .send(Ok(vec![f32::NAN]))
        .expect("the stream is alive");
    assert_eq!(harness.next(), pcm(1, 0, 2));
    assert_eq!(harness.next(), ended(1, 0, TtsError::NonFinite));

    let frames = harness.speak(1, 1);
    harness.wait_entered();
    harness.wait_entered();
    harness.pump.credit(RunId(1), 2);
    send(&frames, 2);
    assert_eq!(harness.next(), pcm(1, 1, 2));
    // A cancel the pump didn't make: the owner leaves without Done and closes the stream.
    harness.pump.tts.cancel();
    drop(frames);
    assert_eq!(harness.next(), ended(1, 1, TtsError::Synthesis));
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
    harness.wait_entered();
    harness.pump.credit(RunId(1), 10);
    harness.pump.cancel(RunId(1));
    // The old stream's audio is loud and the new one's silent, so they can't be mistaken.
    send_value(&first, 4, 0.5);
    harness.assert_pending();

    let second = harness.speak(1, 1);
    harness.wait_entered();
    send(&second, 4);
    assert_eq!(harness.next(), pcm(1, 1, 4), "no new credit needed");
    send(&second, 6);
    assert_eq!(harness.next(), pcm(1, 1, 6));
    send(&second, 4);
    harness.assert_pending();
    harness.pump.cancel(RunId(1));
    drop(second);

    let third = harness.speak(1, 2);
    harness.wait_entered();
    harness.pump.credit(RunId(1), 5);
    send(&third, 3);
    assert_eq!(
        harness.next(),
        pcm(1, 2, 3),
        "the held audio went with the cancel"
    );
    harness.pump.revoke(RunId(1));
    drop(third);

    let fourth = harness.speak(1, 3);
    harness.wait_entered();
    send(&fourth, 1);
    harness.assert_pending();
}

#[test]
fn a_late_credit_from_a_replaced_run_changes_nothing() {
    let mut harness = Harness::new();
    let old = harness.speak(1, 0);
    harness.wait_entered();
    harness.pump.credit(RunId(1), 10);
    let new = harness.speak(2, 0);
    drop(old);
    harness.wait_entered();
    harness.pump.credit(RunId(2), 1);
    harness.pump.credit(RunId(1), 100);
    send(&new, 3);
    assert_eq!(harness.next(), pcm(2, 0, 3));
    send(&new, 3);
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
    // The owner may already have dropped A's stream; either way, none of A comes out.
    let _ = old.send(Ok(vec![0.0; 2]));
    send(&new, 3);
    harness.assert_pending();
    harness.pump.credit(RunId(2), 1);
    assert_eq!(harness.next(), pcm(2, 0, 3));
}

#[test]
fn only_conducted_dispatches_arm_and_first_pcm_disarms_even_when_held_for_credit() {
    for output in [OutputMode::Legacy, OutputMode::Conducted] {
        let mut h = Harness::for_output(output);
        h.runtime.block_on(async { tokio::time::pause() });
        assert_eq!(h.pump.first_pcm_deadline(), None, "no dispatch yet");
        let frames = h.speak(1, 0);
        let due = h.pump.first_pcm_deadline();
        assert_eq!(due.is_some(), output == OutputMode::Conducted);
        send(&frames, 2);
        h.wait_buffered();
        h.assert_pending();
        assert_eq!(
            h.pump.first_pcm_deadline(),
            None,
            "PCM is held, not stalled"
        );
        h.advance(FIRST_PCM_LIMIT);
        if let Some(due) = due {
            assert_eq!(h.expire(due), None);
        }
        h.pump.credit(RunId(1), 2);
        assert_eq!(h.next(), pcm(1, 0, 2));
    }
}

#[test]
fn a_busy_owner_does_not_arm_first_pcm_for_the_rejected_speak() {
    let mut h = Harness::for_output(OutputMode::Conducted);
    let _blocked = h.speak(1, 0);
    h.wait_entered();
    h.pump.cancel(RunId(1));
    let (out, _events) = tokio::sync::mpsc::channel(1);
    let generation = h.pump.tts.generation();
    assert_eq!(
        h.pump.tts.try_speak(Speak {
            segment: 1,
            generation,
            rate: 24_000,
            text: "waiting segment".into(),
            out,
        }),
        Ok(())
    );

    let _rejected = h.speak(1, 2);
    assert_eq!(h.pump.first_pcm_deadline(), None);
    assert_eq!(
        h.next(),
        (
            RunId(1),
            TtsEvent::Failed {
                segment: 2,
                error: TtsError::Synthesis
            }
        )
    );
    assert_eq!(h.pump.tts.generation(), generation);
}

#[test]
fn a_ready_event_at_the_deadline_keeps_its_outcome_without_credit_or_forwarding() {
    enum Ready {
        Audio,
        Done,
        Failed,
        Eof,
    }
    for ready in [Ready::Audio, Ready::Done, Ready::Failed, Ready::Eof] {
        let mut h = Harness::for_output(OutputMode::Conducted);
        h.runtime.block_on(async { tokio::time::pause() });
        let frames = h.speak(1, 4);
        h.wait_entered();
        let due = h.deadline();
        let expected = match ready {
            Ready::Audio => {
                send(&frames, 2);
                pcm(1, 4, 2)
            }
            Ready::Done => {
                drop(frames);
                (
                    RunId(1),
                    TtsEvent::Done {
                        segment: 4,
                        total: 0,
                    },
                )
            }
            Ready::Failed => {
                frames
                    .send(Err(TtsError::Synthesis))
                    .expect("owner is waiting");
                ended(1, 4, TtsError::Synthesis)
            }
            Ready::Eof => {
                h.pump.tts.cancel();
                drop(frames);
                ended(1, 4, TtsError::Synthesis)
            }
        };
        h.wait_buffered();
        let generation = h.pump.tts.generation();
        h.advance(FIRST_PCM_LIMIT);
        let expired = h.runtime.block_on(async {
            for _ in 0..128 {
                tokio::task::consume_budget().await;
            }
            assert!(!tokio::task::coop::has_budget_remaining());
            h.pump.expire_first_pcm(due)
        });
        assert_eq!(expired, None);
        assert_eq!(h.pump.tts.generation(), generation);
        assert_eq!(h.pump.first_pcm_deadline(), None);
        if let Ready::Audio = ready {
            h.assert_pending();
            h.pump.credit(RunId(1), 2);
        }
        assert_eq!(h.next(), expected);
    }
}

#[test]
fn an_unpublished_reserved_send_does_not_delay_first_pcm_expiry() {
    let mut h = Harness::for_output(OutputMode::Conducted);
    h.runtime.block_on(async { tokio::time::pause() });
    let _frames = h.speak(1, 4);
    let due = h.deadline();
    let (out, events) = tokio::sync::mpsc::channel(1);
    let permit = out
        .try_reserve_owned()
        .expect("sender reserved its capacity");
    h.pump.active.as_mut().expect("dispatched Speak").events = events;

    h.advance(FIRST_PCM_LIMIT);
    assert_eq!(h.expire(due), Some((RunId(1), SegmentId(4))));
    assert!(permit.release().is_closed());
    assert_eq!(h.expire(due), None);
}

#[test]
fn first_pcm_timeout_fences_once_while_the_same_owner_is_still_in_next() {
    let mut h = Harness::for_output(OutputMode::Conducted);
    h.runtime.block_on(async { tokio::time::pause() });
    let old = h.speak(1, 0);
    h.wait_entered();
    let due = h.deadline();
    let generation = h.pump.tts.generation();
    let tick = Duration::from_millis(1);
    h.advance(
        FIRST_PCM_LIMIT
            .checked_sub(tick)
            .expect("before the deadline"),
    );
    assert_eq!(h.expire(due), None);
    h.advance(tick);
    assert_eq!(h.expire(due), Some((RunId(1), SegmentId(0))));
    assert_eq!(h.expire(due), None);
    assert_eq!(h.pump.tts.generation(), generation + 1);

    let next = h.speak(2, 0);
    let replacement = h.deadline();
    assert_eq!(h.expire(due), None);
    assert_eq!(h.deadline(), replacement);
    assert_eq!(h.pump.tts.generation(), generation + 1);
    send_value(&old, 2, 0.5);
    drop(old);
    h.wait_entered();
    h.pump.credit(RunId(2), 2);
    send(&next, 2);
    h.wait_buffered();
    assert_eq!(h.next(), pcm(2, 0, 2));
}
