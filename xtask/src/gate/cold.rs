//! Ten input-free cold launches, with the trace already active before process creation.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};

use super::{
    Images,
    capture::{Capture, guarded_wait},
    observation::{self, Kind, Observation},
    present::Present,
};
use crate::platform::{self, Process, gate_capture, input_session::WindowKind};
use crate::session;

pub(in crate::gate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let output = super::retained_output(args)?;
    let images = Images::release(&std::env::current_exe()?)?;
    let foreground = gate_capture::foreground();
    let end = Instant::now() + Duration::from_secs(300);
    let guard = || -> Result<()> {
        ensure!(Instant::now() < end, "cold batch exceeded5min bound");
        ensure!(
            gate_capture::foreground() == foreground,
            "foreground changed during input-free cold batch"
        );
        Ok(())
    };
    fs::write(
        output.join("identity.txt"),
        format!(
            "driver_build_id={}\ndriver_sha256={}\nshell_sha256={}\nbrain_sha256={}\n",
            kumochi_protocol::BUILD_ID,
            gate_capture::file_sha256(&std::env::current_exe()?)?,
            images.shell_sha256,
            images.brain_sha256
        ),
    )?;
    fs::write(
        output.join("environment-before.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    let mut report = File::create_new(output.join("cold.tsv"))?;
    writeln!(
        report,
        "trial\tstatus\tpresent_anchor_estimate_ms\tpresent_conservative_upper_ms\tpresent_qpc\tdisplayed_qpc\tdisplay_conservative_upper_ms"
    )?;
    let mut samples = Vec::new();
    for index in 1..=10 {
        guard()?;
        images.unchanged()?;
        ensure!(
            platform::processes()?.is_empty(),
            "product already running before cold launch"
        );
        let directory = output.join(format!("launch-{index}"));
        fs::create_dir(&directory)?;
        let sample = launch(&images, &directory, &guard);
        match sample {
            Ok(sample) => {
                writeln!(
                    report,
                    "{index}\tmeasured\t{:.6}\t{:.6}\t{}\t{}\t{}",
                    sample.anchor_estimate_ms,
                    sample.upper_ms,
                    sample.present,
                    sample
                        .displayed
                        .map_or_else(|| "NA".to_owned(), |qpc| qpc.to_string()),
                    sample
                        .display_upper_ms
                        .map_or_else(|| "NA".to_owned(), |ms| format!("{ms:.6}"))
                )?;
                samples.push(sample.upper_ms);
            }
            Err(error) => {
                writeln!(report, "{index}\tunmeasured\tNA\tNA\tNA\tNA\tNA")?;
                fs::write(directory.join("result.txt"), format!("{error:#}\n"))?;
                return Err(error.context(format!("cold launch{index}; no retry")));
            }
        }
    }
    samples.sort_by(f64::total_cmp);
    writeln!(
        report,
        "n=10 successful=10 attempted=10 p50_upper_ms={:.6} p95_upper_ms={:.6} max_upper_ms={:.6} budget_ms=400; nearest-rank; no outliers removed",
        samples[4], samples[9], samples[9]
    )?;
    fs::write(
        output.join("environment-after.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    ensure!(
        samples[9] <= 400.0,
        "cold first dock present p95 exceeds400ms"
    );
    Ok(())
}

fn launch(images: &Images, output: &Path, guard: &impl Fn() -> Result<()>) -> Result<Sample> {
    let mut trace = Capture::before_launch(&output.join("cold.csv"), guard)?;
    let mut observer_arg = OsString::from("--gate-observer=");
    let observer = output.join("observer.log");
    observer_arg.push(&observer);
    let launched = platform::launch(&images.shell, 20_000, [observer_arg]);
    let mut child = match launched {
        Ok(child) => child,
        Err(error) => {
            let closed = trace.finish(guard);
            return Err(error.context(format!("prelaunch trace cleanup: {:?}", closed.err())));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(25);
    let pid = child.id();
    let mut window = None;
    let mut helpers = Vec::new();
    let prepared: Result<_> = (|| {
        trace.bind(pid)?;
        let clock = gate_capture::creation_clock(&Process::open(pid)?)?;
        fs::write(output.join("creation.txt"), &clock.evidence)?;
        let product = session::discover(&mut child, WindowKind::ProductProxy)?;
        window = Some(product);
        helpers = session::owned_helpers(pid)?;
        guarded_wait(Duration::from_secs(1), guard)?;
        let mut first_repaint = None;
        let chain = Capture::warm_only(
            pid,
            &output.join("warm.csv"),
            || {
                if first_repaint.is_none() {
                    let cutoff = gate_capture::qpc()?;
                    first_repaint = Some(cutoff);
                    fs::write(
                        output.join("first-harness-repaint.txt"),
                        format!("qpc={cutoff}\n"),
                    )?;
                }
                product.request_gate_repaint()
            },
            guard,
        )?;
        Ok((
            clock,
            chain,
            first_repaint.context("cold repaint cutoff missing")?,
        ))
    })();
    let rows = trace.finish(guard);
    let stopped = session::stop_product(Some(&mut child), window, helpers, deadline);
    let failures = [
        prepared.as_ref().err(),
        rows.as_ref().err(),
        stopped.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .map(|error| format!("{error:#}"))
    .collect::<Vec<_>>();
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    let clock_and_chain = prepared?;
    let rows = rows?;
    stopped?;
    let data = observation::read(&observer, pid)?;
    let seed = rows
        .seed(clock_and_chain.1, clock_and_chain.0.frequency)
        .map_err(|status| anyhow::anyhow!("first dock seed unmeasured: {status:?}"))?;
    fs::write(output.join("seed.txt"), format!("{seed:?}\n"))?;
    let sample = measure(
        &data,
        &seed.present,
        seed.displayed_qpc,
        &clock_and_chain.0,
        clock_and_chain.2,
    )?;
    fs::write(
        output.join("result.txt"),
        format!(
            "{sample:?}\nmethod=creation to PresentMon's first-present seed, derived from the first dock CSV row and bounded by at least one Dock paint/completion. Every containing interval must begin before the first harness repaint; overlapping startup intervals are allowed. Missing or unbounded seeds are unmeasured; later ordinary/warm-up rows are never substituted. Seed provenance is retained in seed.txt; display latency is reported separately, not gated. Prelaunch name filtering remains; suspended launch was not adopted. Latencies use OS-reported creation UTC mapped to QPC, with the full clock bracket retained. anchor_estimate_ms is not a physical lower bound: OS creation-timestamp granularity is outside the anchor bracket; only present conservative upper_ms gates.\n"
        ),
    )?;
    Ok(sample)
}

#[derive(Debug)]
struct Sample {
    anchor_estimate_ms: f64,
    upper_ms: f64,
    present: i64,
    displayed: Option<i64>,
    display_upper_ms: Option<f64>,
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "bounded latency deltas, never absolute QPCs, are converted to milliseconds"
)]
fn measure(
    data: &Observation,
    row: &Present,
    displayed: Option<i64>,
    clock: &gate_capture::CreationClock,
    first_repaint: i64,
) -> Result<Sample> {
    ensure!(
        data.startup
            .as_ref()
            .context("cold startup missing")?
            .frequency
            == clock.frequency,
        "cold clock domains differ"
    );
    let mut frames = data.frames.iter().filter(|frame| {
        frame.kind == Kind::Dock
            && frame.paint_qpc <= row.qpc
            && frame.completion_qpc.is_some_and(|end| row.qpc <= end)
    });
    let frame = frames
        .next()
        .context("first dock seed has no frame bound; no later substitution")?;
    ensure!(!row.dropped, "first dock seed is dropped");
    ensure!(
        std::iter::once(frame)
            .chain(frames)
            .all(|frame| frame.paint_qpc < first_repaint),
        "first dock seed has a frame painted at or after the harness repaint"
    );
    ensure!(
        row.qpc >= clock.latest,
        "cold present precedes process creation"
    );
    Ok(Sample {
        anchor_estimate_ms: (row.qpc - clock.latest) as f64 * 1000.0 / clock.frequency as f64,
        upper_ms: (row.qpc - clock.earliest) as f64 * 1000.0 / clock.frequency as f64,
        present: row.qpc,
        displayed,
        display_upper_ms: displayed
            .map(|qpc| (qpc - clock.earliest) as f64 * 1000.0 / clock.frequency as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::observation::{Frame, Startup};

    #[test]
    fn first_unbounded_present_is_not_replaced_by_a_later_good_one() {
        let mut data = Observation::default();
        data.startup = Some(Startup {
            frequency: 1000,
            work_area: [0, 0, 100, 100],
            dock: [0, 0, 4, 2],
            overlay: [0, 0, 10, 10],
            panel: [0, 2, 10, 10],
            panel_hwnd: 2,
        });
        data.frames = vec![
            Frame {
                id: 1,
                paint_qpc: 100,
                completion_qpc: Some(110),
                kind: Kind::Dock,
                hover: 0,
                geometry: 0,
            },
            Frame {
                id: 2,
                paint_qpc: 120,
                completion_qpc: Some(130),
                kind: Kind::Dock,
                hover: 0,
                geometry: 1,
            },
        ];
        let clock = gate_capture::CreationClock {
            earliest: 89,
            latest: 90,
            frequency: 1000,
            evidence: String::new(),
        };
        let mut seed = Present {
            qpc: 111,
            swapchain: 5,
            dropped: false,
            display_after_ms: None,
        };
        assert!(measure(&data, &seed, None, &clock, 115).is_err());
        seed.qpc = 105;
        let sample = measure(&data, &seed, Some(106), &clock, 115).unwrap();
        assert_eq!(sample.present, 105);
        assert_eq!(sample.displayed, Some(106));
        assert!((sample.anchor_estimate_ms - 15.0).abs() < f64::EPSILON);
        assert!((sample.upper_ms - 16.0).abs() < f64::EPSILON);
        assert_eq!(sample.display_upper_ms, Some(17.0));
        assert_eq!(
            measure(&data, &seed, None, &clock, 115).unwrap().displayed,
            None
        );
        seed.qpc = 125;
        assert!(
            measure(&data, &seed, None, &clock, 115).is_err(),
            "a warm repaint cannot supply a seed"
        );
        assert_eq!(
            measure(&data, &seed, None, &clock, 135).unwrap().present,
            125,
            "a proved first-present seed may match a later recorded startup paint"
        );
        seed.qpc = 105;
        assert!(
            measure(&data, &seed, None, &clock, 100).is_err(),
            "paint at the first repaint is ineligible"
        );
        seed.dropped = true;
        assert!(measure(&data, &seed, None, &clock, 115).is_err());
        seed.dropped = false;
        data.frames[1].paint_qpc = 104;
        assert_eq!(
            measure(&data, &seed, None, &clock, 115).unwrap().present,
            105,
            "overlapping startup bounds describe the same first present"
        );
        assert!(
            measure(&data, &seed, None, &clock, 103).is_err(),
            "a post-repaint overlapping frame cannot be ignored"
        );
        data.frames[1].paint_qpc = 120;
        data.frames[0].completion_qpc = None;
        assert!(measure(&data, &seed, None, &clock, 115).is_err());
    }
}
