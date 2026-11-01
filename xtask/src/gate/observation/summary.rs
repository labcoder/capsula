//! Exact bounded frame/display joins and complete-sample gate reports.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};

use super::{Frame, Kind, Observation, load};
use crate::gate::present::{Join, PresentRows};

pub(in crate::gate) fn summarize_keyboard(
    canvas_pairs: &[(i64, i64)],
    path: &Path,
    pid: u32,
    rows: &mut PresentRows,
    dock_chain: u64,
    output: &Path,
) -> Result<()> {
    let (observation, validation) = load(path, pid);
    let frequency = observation.startup.map_or(0, |value| value.frequency);
    let mut report =
        String::from("keyboard callback-to-display; submission markers are not display feedback\n");
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let mut chords = Vec::new();
    let mut successful = 0;
    for (index, &(start, canvas)) in canvas_pairs.iter().enumerate() {
        let role = match index {
            0..=99 => "chord",
            100 => "hold",
            _ => "extra",
        };
        write!(
            report,
            "sample={} role={role} start_qpc={start} canvas_qpc={canvas} ",
            index + 1
        )?;
        let frame = if start <= 0 || canvas < start {
            Err("InvalidCallback")
        } else {
            dock_frame(&observation, canvas)
        };
        let ticks = match frame {
            Ok(frame) if validation.is_ok() => sample(
                &mut report,
                frame,
                start,
                rows.join(
                    dock_chain,
                    frame.paint_qpc,
                    frame.completion_qpc.unwrap_or(0),
                    frequency,
                ),
                frequency,
            )?,
            Ok(_) => {
                writeln!(report, "status=ObservationInvalid")?;
                None
            }
            Err(status) => {
                writeln!(report, "status={status}")?;
                None
            }
        };
        if let Some(ticks) = ticks {
            successful += 1;
            if index < 100 {
                chords.push(ticks);
            }
        }
    }
    let budget_pass = statistics(
        &mut report,
        "chords",
        &mut chords,
        canvas_pairs.len().min(100),
        frequency,
        7,
    )?;
    writeln!(
        report,
        "expected_chords=100 actual_pairs={} expected_hold_rows=1",
        canvas_pairs.len()
    )?;
    let pass = validation.is_ok() && canvas_pairs.len() == 101 && successful == 101 && budget_pass;
    writeln!(
        report,
        "total_successful={successful} attempted={} expected=101 gate={}",
        canvas_pairs.len(),
        if pass { "pass" } else { "unmeasured_or_fail" }
    )?;
    save_report(&output.join("keyboard-display.txt"), &report)?;
    ensure!(
        pass,
        "keyboard display gate incomplete or over budget; see {}",
        output.display()
    );
    Ok(())
}

fn dock_frame(observation: &Observation, canvas: i64) -> std::result::Result<&Frame, &'static str> {
    let mut frames = observation.frames.iter().filter(|frame| {
        frame.kind == Kind::Dock
            && frame
                .completion_qpc
                .is_some_and(|end| (frame.paint_qpc..=end).contains(&canvas))
    });
    match (frames.next(), frames.next()) {
        (Some(frame), None) => Ok(frame),
        (None, _) => Err("MissingFrame"),
        _ => Err("AmbiguousFrame"),
    }
}

pub(in crate::gate) fn summarize_hover(
    path: &Path,
    pid: u32,
    rows: &mut PresentRows,
    dock_chain: u64,
    output: &Path,
) -> Result<()> {
    let (observation, validation) = load(path, pid);
    let frequency = observation.startup.map_or(0, |value| value.frequency);
    let mut report =
        String::from("hover callback-to-display; submission markers are not display feedback\n");
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let mut pinned = None;
    let mut latencies = Vec::new();
    let mut attempted = 0;
    for hover in observation.hovers.iter().filter(|hover| hover.visible) {
        attempted += 1;
        write!(report, "hover={} start_qpc={} ", hover.id, hover.qpc)?;
        let mut frames = observation
            .frames
            .iter()
            .filter(|frame| frame.kind == Kind::Panel && frame.hover == hover.id);
        let Some(frame) = frames.next() else {
            writeln!(report, "status=MissingFrame")?;
            continue;
        };
        if frames
            .next()
            .is_some_and(|next| next.paint_qpc == frame.paint_qpc)
        {
            writeln!(report, "status=AmbiguousFirstFrame")?;
            continue;
        }
        if validation.is_err() {
            writeln!(report, "status=ObservationInvalid")?;
            continue;
        }
        let Some(end) = frame.completion_qpc else {
            writeln!(report, "status=MissingCompletion frame={}", frame.id)?;
            continue;
        };
        if frame.paint_qpc < hover.qpc {
            writeln!(report, "status=InvalidHoverClock")?;
            continue;
        }
        let chain = match rows.non_dock_swapchain(dock_chain, frame.paint_qpc, end) {
            Ok(chain) => chain,
            Err(status) => {
                writeln!(report, "status={status:?} frame={}", frame.id)?;
                continue;
            }
        };
        if pinned.is_some_and(|value| value != chain) {
            writeln!(report, "status=PanelSwapchainChanged frame={}", frame.id)?;
            continue;
        }
        pinned = Some(chain);
        let joined = rows.join(chain, frame.paint_qpc, end, frequency);
        if let Some(ticks) = sample(&mut report, frame, hover.qpc, joined, frequency)? {
            latencies.push(ticks);
        }
    }
    let budget_pass = statistics(
        &mut report,
        "hover",
        &mut latencies,
        attempted,
        frequency,
        8,
    )?;
    let pass = validation.is_ok() && attempted == 100 && latencies.len() == 100 && budget_pass;
    writeln!(
        report,
        "expected_enters=100 panel_swapchain={pinned:?} gate={}",
        if pass { "pass" } else { "unmeasured_or_fail" }
    )?;
    save_report(&output.join("hover-display.txt"), &report)?;
    ensure!(
        pass,
        "hover display gate incomplete or over budget; see {}",
        output.display()
    );
    Ok(())
}

fn sample(
    report: &mut String,
    frame: &Frame,
    start: i64,
    joined: Join,
    frequency: i64,
) -> Result<Option<i64>> {
    write!(
        report,
        "frame={} paint_qpc={} completion_qpc={:?} ",
        frame.id, frame.paint_qpc, frame.completion_qpc
    )?;
    match joined {
        Join::Displayed {
            present_qpc,
            displayed_qpc,
        } => {
            if let Some(ticks) = displayed_qpc.checked_sub(start).filter(|ticks| *ticks > 0) {
                writeln!(
                    report,
                    "status=Displayed present_qpc={present_qpc} displayed_qpc={displayed_qpc} latency_ms={}",
                    milliseconds(ticks, frequency)?
                )?;
                return Ok(Some(ticks));
            }
            writeln!(report, "status=InvalidDisplayClock")?;
        }
        status => writeln!(report, "status={status:?}")?,
    }
    Ok(None)
}

fn milliseconds(ticks: i64, frequency: i64) -> Result<String> {
    ensure!(ticks >= 0 && frequency > 0, "invalid latency clock");
    let nanos = (u128::try_from(ticks)? * 1_000_000_000) / u128::try_from(frequency)?;
    Ok(format!("{}.{:06}", nanos / 1_000_000, nanos % 1_000_000))
}

fn statistics(
    report: &mut String,
    label: &str,
    ticks: &mut [i64],
    attempted: usize,
    frequency: i64,
    budget_ms: i64,
) -> Result<bool> {
    if ticks.is_empty() {
        writeln!(
            report,
            "summary={label} n=0 successful=0 attempted={attempted} p50_ms=unmeasured p95_ms=unmeasured max_ms=unmeasured"
        )?;
        return Ok(false);
    }
    ticks.sort_unstable();
    let p50 = ticks[(ticks.len() * 50).div_ceil(100) - 1];
    let p95 = ticks[(ticks.len() * 95).div_ceil(100) - 1];
    let max = ticks[ticks.len() - 1];
    let pass = i128::from(p95) * 1000 <= i128::from(frequency) * i128::from(budget_ms);
    writeln!(
        report,
        "summary={label} n={} successful={} attempted={attempted} p50_ms={} p95_ms={} max_ms={} budget_ms={budget_ms} budget_pass={pass}",
        ticks.len(),
        ticks.len(),
        milliseconds(p50, frequency)?,
        milliseconds(p95, frequency)?,
        milliseconds(max, frequency)?
    )?;
    Ok(pass)
}

fn save_report(path: &Path, report: &str) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(report.as_bytes())?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        read,
        tests::{COMMIT, GEOMETRY, STARTUP, directory, finish},
    };
    use super::*;
    use std::fs;

    const CSV_HEADER: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime\n";

    fn scenario(directory: &Path, kind: Kind) -> Vec<(i64, i64)> {
        let hover = kind == Kind::Panel;
        let count = if hover { 100 } else { 101 };
        let mut text = format!(
            "{GEOMETRY}{COMMIT}{STARTUP}paint frame=1 qpc=12 kind=panel hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\n"
        );
        let mut csv = CSV_HEADER.to_owned();
        let mut pairs = Vec::new();
        for index in 0..count {
            let start = 100 + index * 20;
            let paint = start + 1;
            if hover {
                writeln!(text, "hover id={} qpc={start} visible=true", index + 1).unwrap();
            }
            let (kind, epoch, geometry, hover_id, chain) = if hover {
                ("panel", 0, 0, index + 1, 20)
            } else {
                ("dock", 1, 1, 0, 10)
            };
            writeln!(text, "paint frame={} qpc={paint} kind={kind} hover={hover_id} drawn_epoch={epoch} drawn_geometry={geometry} mask_epoch=1\ndrawn frame={} qpc={}", index + 2, index + 2, paint + 4).unwrap();
            let display_ms = if !hover && index == 100 { 50 } else { 1 };
            writeln!(
                csv,
                "capsula.exe,42,0x{chain:X},DXGI,0,0,0,0.1,0,0,0,Composed: Flip,0,{display_ms},0,0,0,0,{}",
                paint + 1
            )
            .unwrap();
            pairs.push((start, paint + 1));
        }
        fs::write(
            directory.join("observer.log"),
            finish(text, usize::try_from(count).unwrap() + 1, 1),
        )
        .unwrap();
        fs::write(directory.join("present.csv"), csv).unwrap();
        pairs
    }

    #[test]
    fn keyboard_reports_hold_separately_and_preserves_missing_joins() {
        let directory = directory();
        let pairs = scenario(&directory, Kind::Dock);
        let mut rows = crate::gate::present::read(&directory.join("present.csv"), 42).unwrap();
        summarize_keyboard(
            &pairs,
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("keyboard-display.txt")).unwrap();
        assert!(
            report.contains("sample=101 role=hold"),
            "hold stays outside the chord percentile"
        );
        assert!(
            report.contains("latency_ms=52.000000"),
            "slow hold is retained separately"
        );
        assert!(report.contains("summary=chords n=100 successful=100 attempted=100 p50_ms=3.000000 p95_ms=3.000000 max_ms=3.000000"), "exact complete chord statistics");
        let mut observation = read(&directory.join("observer.log"), 42).unwrap();
        assert_eq!(
            dock_frame(&observation, 105).map(|frame| frame.id),
            Ok(2),
            "completion boundary is inclusive"
        );
        observation.frames[1].completion_qpc = Some(125);
        assert!(
            matches!(dock_frame(&observation, 122), Err("AmbiguousFrame")),
            "overlapping bounds cannot select a frame"
        );
        fs::remove_file(directory.join("keyboard-display.txt")).unwrap();
        let mut rows = crate::gate::present::read(&directory.join("present.csv"), 42).unwrap();
        let mut incomplete = pairs;
        incomplete[0] = (1, 2);
        assert!(
            summarize_keyboard(
                &incomplete,
                &directory.join("observer.log"),
                42,
                &mut rows,
                10,
                &directory
            )
            .is_err(),
            "missing frame prevents gate success"
        );
        let report = fs::read_to_string(directory.join("keyboard-display.txt")).unwrap();
        assert!(
            report.contains("status=MissingFrame")
                && report.contains("total_successful=100 attempted=101"),
            "failed sample remains in denominator and report"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hover_pins_only_unique_panel_chain_and_uses_nearest_rank_percentiles() {
        let directory = directory();
        scenario(&directory, Kind::Panel);
        let csv_path = directory.join("present.csv");
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        summarize_hover(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
        assert!(
            report.contains("n=100 successful=100 attempted=100 p50_ms=3.000000 p95_ms=3.000000"),
            "all enters have exact display latency"
        );
        fs::remove_file(directory.join("hover-display.txt")).unwrap();
        let csv = fs::read_to_string(&csv_path).unwrap();
        fs::write(&csv_path, csv.replacen("0x14", "0x15", 1)).unwrap();
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        assert!(
            summarize_hover(
                &directory.join("observer.log"),
                42,
                &mut rows,
                10,
                &directory
            )
            .is_err(),
            "a changed chain cannot pass"
        );
        assert!(
            fs::read_to_string(directory.join("hover-display.txt"))
                .unwrap()
                .contains("status=PanelSwapchainChanged"),
            "chain failure evidence is retained"
        );
        let mut ticks = (1..=100).rev().collect::<Vec<_>>();
        let mut report = String::new();
        assert!(
            !statistics(&mut report, "test", &mut ticks, 100, 1000, 8).unwrap(),
            "95 ms misses the budget"
        );
        assert!(
            report.contains("p50_ms=50.000000 p95_ms=95.000000 max_ms=100.000000"),
            "nearest-rank statistics retain all samples"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
