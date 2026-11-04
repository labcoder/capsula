//! Manual quiet-Rez before-and-after timing of the data pipe's scene hot path: the reader thread
//! decoding and delivering frames from a real local pipe, then the UI's batch collection and
//! per-event disposal. Each method runs alone in its own test process, so the global dropper is
//! unstarted for main's baseline and started for the current path.

use std::io::{BufReader, Write as _};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    DATA_WINDOW, DataToShell, RunId, SceneHeader, SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{Record, SceneEvent, Step, deferred, parse_record, text::Bounded};

use super::{Counted, EVENT_BATCH, LinkEvent, READ_BUFFER};
use crate::platform::{PipeClient, test_pair};

const WARMUPS: usize = 5;
const SAMPLES: usize = 64;
/// Maximum frames per sample, all taken by one UI batch within the wire window.
const MAX_FRAMES: usize = 64;
const GENERATION: u64 = 1;
const PATIENCE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum Method {
    /// main 8b12a0a: raw frames in `LinkEvent::Data`, a raw `Vec` batch, destructors on the UI.
    Main,
    /// This branch: `read_data` protects scene frames and `batch` protects the UI batch.
    Current,
}

/// main 8b12a0a's `read_data`, verbatim.
fn main_read_data(
    data: &PipeClient,
    control: &PipeClient,
    events: &Sender<LinkEvent>,
    generation: u64,
) {
    let mut reader = Counted {
        inner: BufReader::with_capacity(READ_BUFFER, data),
        bytes: 0,
    };
    loop {
        reader.bytes = 0;
        match frame::read::<DataToShell>(&mut reader, Pipe::Data) {
            Ok(Some(message)) => {
                // At most the data cap and a header.
                let bytes = u32::try_from(reader.bytes).unwrap_or(u32::MAX);
                if events
                    .send_blocking(LinkEvent::Data {
                        generation,
                        message,
                        bytes,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => break,
            Err(FrameError::Io(error)) => {
                tracing::debug!(%error, "data pipe closed");
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "the brain broke the data pipe's protocol");
                break;
            }
        }
    }
    control.abort();
}

/// The UI's side of one update: collect the batch, take each event out and dispose of it, as
/// `link_events` does once the presenter has applied it. Presenter work is not included.
fn ui_batch(method: Method, first: LinkEvent, rest: &Receiver<LinkEvent>) {
    match method {
        Method::Main => {
            // main 8b12a0a's `listen` collection, verbatim.
            let mut events = vec![first];
            while events.len() < EVENT_BATCH
                && let Ok(event) = rest.try_recv()
            {
                events.push(event);
            }
            for event in events {
                drop(event);
            }
        }
        Method::Current => {
            let mut events = super::batch(first, rest);
            for event in events.drain(..) {
                drop(event);
            }
            events.attribute(events.capacity() * size_of::<LinkEvent>());
        }
    }
}

fn step(json: &str) -> Step {
    let Ok(Record::Step(step)) = parse_record(json.as_bytes()) else {
        panic!("a step record");
    };
    step
}

/// Identical text, tiny/cap-sized commits and snapshot frames for both methods.
fn workloads() -> Vec<(&'static str, DataToShell)> {
    let shapes =
        (0..300).map(|i| format!(r#"{{"shape":{{"id":"s{i}","kind":"rect","box":[0,0,1,1]}}}}"#));
    let tracks = (0..128).map(|i| {
        format!(r#"{{"anim":{{"target":"s{i}","track":"opacity","keys":[[0,0],[60000,1]]}}}}"#)
    });
    let large = step(&format!(
        r#"{{"step":{{"id":"large","put":[{}]}}}}"#,
        shapes.chain(tracks).collect::<Vec<_>>().join(",")
    ));
    let tiny = step(
        r#"{"step":{"id":"tiny","put":[{"shape":{"id":"t","kind":"rect","box":[0,0,10,10]}}]}}"#,
    );
    let Ok(Record::Begin(begin)) = parse_record(
        br#"{"begin":{"v":1,"scene":"synthetic","surface":"overlay","alt":"synthetic"}}"#,
    ) else {
        panic!("a begin record");
    };
    let commit = |step| DataToShell::SceneStepCommit {
        run: RunId(1),
        scene: SceneId(1),
        index: 0,
        step,
    };
    let snapshot = DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene: SceneId(1),
            revision: 1,
            ordinal: 0,
            header: Some(SceneHeader {
                begin,
                layout_version: 1,
            }),
            events: Bounded::new(vec![SceneEvent::Step {
                revision: 1,
                step: std::sync::Arc::new(large.clone()),
            }])
            .expect("one event"),
            last: true,
        },
    };
    vec![
        ("assistant_text", DataToShell::AssistantText { run: RunId(1), delta: "delta ".repeat(4) }),
        ("commit_rect1", commit(tiny)),
        ("commit_rects300_tracks128", commit(large)),
        ("snapshot_rects300_tracks128", snapshot),
    ]
}

fn pair(label: &str) -> (PipeClient, PipeClient) {
    let name = format!(
        r"\\.\pipe\{}-measure-{}-{label}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    test_pair(&name).expect("a test pipe")
}

fn until(done: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "{what} in time");
        std::hint::spin_loop();
    }
}

/// p50, p95 and maximum, at the indices the presenter measurements use for 64 samples.
fn stats(mut samples: Vec<u128>) -> String {
    samples.sort_unstable();
    format!(
        "{}/{}/{}",
        samples[samples.len() / 2 - 1],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1]
    )
}

#[expect(clippy::print_stdout, reason = "manual Rez measurement output")]
fn run(method: Method, label: &str) {
    for (workload, message) in workloads() {
        let frame = frame::encode(&message, Pipe::Data).expect("a bounded frame");
        let frames = MAX_FRAMES
            .min(usize::try_from(DATA_WINDOW).expect("wire window fits usize") / frame.len());
        assert!(frames > 0, "one frame fits the data window");
        drop(message);
        let (brain_data, shell_data) = pair(&format!("{label}-{workload}-data"));
        let (brain_control, shell_control) = pair(&format!("{label}-{workload}-control"));
        let (events, received) = async_channel::unbounded();
        let reader = thread::spawn(move || match method {
            Method::Main => main_read_data(&shell_data, &shell_control, &events, GENERATION),
            Method::Current => super::read_data(&shell_data, &shell_control, &events, GENERATION),
        });
        let (mut wire, mut ui, mut drain, mut peak) = (Vec::new(), Vec::new(), Vec::new(), 0);
        let mut writer = &brain_data;
        for sample in 0..WARMUPS + SAMPLES {
            deferred::take_peak();
            let start = Instant::now();
            for _ in 0..frames {
                writer.write_all(&frame).expect("a frame written");
            }
            until(|| received.len() == frames, "every frame delivered");
            let delivered = start.elapsed().as_nanos();
            let start = Instant::now();
            let first = received.recv_blocking().expect("a delivered frame");
            ui_batch(method, first, &received);
            let disposed = start.elapsed().as_nanos();
            assert!(received.is_empty(), "one UI batch takes every frame");
            let start = Instant::now();
            until(
                || deferred::pending().values == 0,
                "the dropper backlog cleared",
            );
            let drained = start.elapsed().as_nanos();
            if sample >= WARMUPS {
                wire.push(delivered);
                ui.push(disposed);
                drain.push(drained);
                peak = peak.max(deferred::take_peak());
            }
        }
        drop(brain_data);
        reader.join().expect("the reader ends with its pipe");
        drop(brain_control);
        println!(
            "link_hot_path method={label} workload={workload} frames={frames} frame_bytes={} \
             samples={SAMPLES} warmups={WARMUPS} wire_ns_p50/p95/max={} ui_ns_p50/p95/max={} \
             drain_ns_p50/p95/max={} queued_peak_bytes={peak}; wire: pre-encoded writes on a real \
             local pipe through read_data's decode and channel delivery; ui: recv, batch, \
             per-event disposal and attribution, no presenter apply; drain: dropper backlog \
             after the UI returned",
            frame.len(),
            stats(wire),
            stats(ui),
            stats(drain),
        );
    }
}

#[test]
#[ignore = "quiet-Rez baseline; run with --exact --ignored alone in its own process"]
fn measure_link_hot_path_main() {
    assert!(
        !deferred::running(),
        "run alone: main's baseline needs an unstarted dropper"
    );
    run(Method::Main, "main8b12a0a");
}

#[test]
#[ignore = "quiet-Rez timing; run with --exact --ignored alone in its own process"]
fn measure_link_hot_path_current() {
    deferred::start().expect("the dropper starts before intake");
    run(Method::Current, "current");
}
