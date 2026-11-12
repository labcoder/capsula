//! Exact bounded frame/present joins with display latency reported separately.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};

use super::{Frame, Hover, Kind, Observation, load};
use crate::gate::present::{Join, MatchedPresent, PresentRows};

/// The first panel frame drawn for one visible panel change.
fn first_panel_frame(
    observation: &Observation,
    change: Hover,
) -> std::result::Result<&Frame, &'static str> {
    let mut frames = observation
        .frames
        .iter()
        .filter(|frame| frame.kind == Kind::Panel && frame.hover == change.id);
    let frame = frames.next().ok_or("MissingFrame")?;
    if frames
        .next()
        .is_some_and(|next| next.paint_qpc == frame.paint_qpc)
    {
        return Err("AmbiguousFirstFrame");
    }
    if frame.paint_qpc < change.qpc {
        return Err("InvalidPanelClock");
    }
    Ok(frame)
}

/// The frame's completion and its panel swapchain, which must stay the one first seen.
fn pinned_panel_chain(
    rows: &PresentRows,
    dock_chain: u64,
    frame: &Frame,
    frequency: i64,
    pinned: &mut Option<u64>,
) -> std::result::Result<(i64, u64), String> {
    let end = frame.completion_qpc.ok_or("MissingCompletion")?;
    let chain = rows
        .non_dock_swapchain(dock_chain, frame.paint_qpc, end, frequency)
        .map_err(|status| format!("{status:?}"))?;
    if pinned.is_some_and(|value| value != chain) {
        return Err("PanelSwapchainChanged".to_owned());
    }
    *pinned = Some(chain);
    Ok((end, chain))
}

pub(in crate::gate) fn summarize_hover(
    path: &Path,
    pid: u32,
    rows: &mut PresentRows,
    dock_chain: u64,
    output: &Path,
    injections: &[i64],
) -> Result<()> {
    let (observation, validation) = load(path, pid);
    let frequency = observation.startup.map_or(0, |value| value.frequency);
    let mut report = String::from(
        "pointer-entry dispatch QPC-to-present gate; excludes OS routing and the UI thread's message-queue wait before dispatch, includes the presence event channel to the handler; SendInput-, state-entry- and display-to-present reported separately, not gated\n",
    );
    seed_report(&mut report, rows, frequency)?;
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let mut pinned = None;
    let mut latencies = Vec::new();
    let mut displays = Vec::new();
    let mut injection_latencies = Vec::new();
    let mut state_latencies = Vec::new();
    let mut attempted = 0;
    let mut previous_start = 0;
    for change in observation.hovers.iter().filter(|change| change.visible) {
        attempted += 1;
        write!(
            report,
            "hover={attempted} panel_change={} state_qpc={} entry_qpc={} ",
            change.id,
            change.qpc,
            change.click_qpc.unwrap_or(0)
        )?;
        let entry = change.click_qpc.filter(|entry| *entry > previous_start);
        let start = entry.unwrap_or(change.qpc);
        let since = std::mem::replace(&mut previous_start, change.qpc);
        let frame = match first_panel_frame(&observation, *change) {
            Ok(frame) => frame,
            Err(status) => {
                writeln!(report, "status={status}")?;
                continue;
            }
        };
        if validation.is_err() {
            writeln!(report, "status=ObservationInvalid")?;
            continue;
        }
        let (end, chain) = match pinned_panel_chain(rows, dock_chain, frame, frequency, &mut pinned)
        {
            Ok(found) => found,
            Err(status) => {
                writeln!(report, "status={status} frame={}", frame.id)?;
                continue;
            }
        };
        let joined = rows.join_present(chain, frame.paint_qpc, end, frequency);
        injection_latencies.extend(injection_sample(
            &mut report,
            since,
            start,
            injections,
            &joined,
            frequency,
        )?);
        let state = joined.as_ref().map(|row| row.present.qpc - change.qpc);
        state_latencies.extend(state.ok().filter(|ticks| *ticks > 0));
        let Some(entry) = entry else {
            writeln!(report, "status=MissingEntry")?;
            continue;
        };
        if let Some((ticks, display)) = sample(&mut report, frame, entry, joined, frequency)? {
            latencies.push(ticks);
            displays.extend(display);
        }
    }
    let budget_pass = statistics(
        &mut report,
        "hover",
        &mut latencies,
        attempted,
        frequency,
        Some(8),
    )?;
    for (label, ticks) in [
        ("hover_display", &mut displays),
        ("hover_send_input", &mut injection_latencies),
        ("hover_state", &mut state_latencies),
    ] {
        statistics(&mut report, label, ticks, attempted, frequency, None)?;
    }
    let pass = validation.is_ok() && attempted == 100 && latencies.len() == 100 && budget_pass;
    writeln!(
        report,
        "expected_entries=100 shown={attempted} panel_swapchain={pinned:?} gate={}",
        if pass { "pass" } else { "unmeasured_or_fail" }
    )?;
    save_report(&output.join("hover-display.txt"), &report)?;
    ensure!(
        pass,
        "hover present gate incomplete or over budget; see {}",
        output.display()
    );
    Ok(())
}

fn injection_sample(
    report: &mut String,
    previous_click: i64,
    click: i64,
    injections: &[i64],
    joined: &std::result::Result<MatchedPresent, Join>,
    frequency: i64,
) -> Result<Option<i64>> {
    let mut starts = injections
        .iter()
        .copied()
        .filter(|start| *start > previous_click && *start <= click);
    let start = starts.next().filter(|_| starts.next().is_none());
    let ticks = start
        .zip(joined.as_ref().ok())
        .and_then(|(start, row)| row.present.qpc.checked_sub(start))
        .filter(|ticks| *ticks > 0 && frequency > 0);
    if let (Some(start), Some(ticks)) = (start, ticks) {
        write!(
            report,
            "send_input_qpc={start} send_input_to_present_ms={} ",
            milliseconds(ticks, frequency)?
        )?;
    } else {
        write!(
            report,
            "send_input_qpc=unmeasured send_input_to_present_ms=unmeasured "
        )?;
    }
    Ok(ticks)
}

fn seed_report(report: &mut String, rows: &PresentRows, frequency: i64) -> Result<()> {
    writeln!(report, "seed_qpc_frequency={frequency}")?;
    for source in rows.seed_sources() {
        writeln!(
            report,
            "seed_source={source:?} derivation={:?}",
            rows.seed(source.present.swapchain, frequency)
        )?;
    }
    Ok(())
}

fn sample(
    report: &mut String,
    frame: &Frame,
    start: i64,
    joined: std::result::Result<MatchedPresent, Join>,
    frequency: i64,
) -> Result<Option<(i64, Option<i64>)>> {
    write!(
        report,
        "frame={} paint_qpc={} completion_qpc={:?} ",
        frame.id, frame.paint_qpc, frame.completion_qpc
    )?;
    match joined {
        Ok(row) => {
            write!(
                report,
                "source={:?} present={:?} present_qpc={} ",
                row.source, row.present, row.present.qpc
            )?;
            let Some(ticks) = row
                .present
                .qpc
                .checked_sub(start)
                .filter(|ticks| *ticks > 0)
            else {
                writeln!(report, "status=InvalidPresentClock")?;
                return Ok(None);
            };
            if frequency <= 0 {
                writeln!(report, "status=InvalidClock")?;
                return Ok(None);
            }
            write!(
                report,
                "status=Presented present_latency_ms={} ",
                milliseconds(ticks, frequency)?
            )?;
            let displayed = row.displayed_qpc;
            let display_ticks = displayed.and_then(|qpc| qpc.checked_sub(start));
            if let (Some(displayed_qpc), Some(display_ticks)) = (displayed, display_ticks) {
                writeln!(
                    report,
                    "display_status=Displayed displayed_qpc={displayed_qpc} display_latency_ms={}",
                    milliseconds(display_ticks, frequency)?
                )?;
            } else {
                writeln!(
                    report,
                    "display_status=Unavailable displayed_qpc=unmeasured display_latency_ms=unmeasured"
                )?;
            }
            return Ok(Some((ticks, display_ticks)));
        }
        Err(status) => writeln!(report, "status={status:?}")?,
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
    budget_ms: Option<i64>,
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
    write!(
        report,
        "summary={label} n={} successful={} attempted={attempted} p50_ms={} p95_ms={} max_ms={}",
        ticks.len(),
        ticks.len(),
        milliseconds(p50, frequency)?,
        milliseconds(p95, frequency)?,
        milliseconds(max, frequency)?
    )?;
    let pass = budget_ms
        .is_none_or(|budget| i128::from(p95) * 1000 <= i128::from(frequency) * i128::from(budget));
    if let Some(budget) = budget_ms {
        write!(report, " budget_ms={budget} budget_pass={pass}")?;
    }
    writeln!(report)?;
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
    use super::super::tests::{COMMIT, GEOMETRY, STARTUP, directory, finish};
    use super::*;
    use std::fs;

    const CSV_HEADER: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime\n";

    fn scenario(directory: &Path) {
        let mut text = format!(
            "{GEOMETRY}{COMMIT}{STARTUP}paint frame=1 qpc=12 kind=panel hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\n"
        );
        let mut csv = CSV_HEADER.to_owned();
        for index in 0..100 {
            let start = 100 + index * 20;
            let paint = start + 1;
            writeln!(
                text,
                "click qpc={start} point=2,2 mask_epoch=1 marker=0x43505331"
            )
            .unwrap();
            writeln!(
                text,
                "hover id={} qpc={paint} visible=true click_qpc={start}",
                index + 1
            )
            .unwrap();
            writeln!(text, "paint frame={} qpc={paint} kind=panel hover={} drawn_epoch=0 drawn_geometry=0 mask_epoch=1\ndrawn frame={} qpc={}", index + 2, index + 1, index + 2, paint + 4).unwrap();
            writeln!(
                csv,
                "app.exe,42,0x14,DXGI,0,0,0,0.1,0,0,0,Composed: Flip,0,13,0,0,0,0,{}",
                paint + 1
            )
            .unwrap();
        }
        fs::write(directory.join("observer.log"), finish(text, 101, 1)).unwrap();
        fs::write(directory.join("present.csv"), csv).unwrap();
    }

    #[test]
    fn hover_pins_only_unique_panel_chain_and_uses_nearest_rank_percentiles() {
        let directory = directory();
        scenario(&directory);
        let injections = (0..100).map(|index| 90 + index * 20).collect::<Vec<i64>>();
        let csv_path = directory.join("present.csv");
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        summarize_hover(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
            &injections,
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
        assert!(
            report.contains("summary=hover n=100 successful=100 attempted=100 p50_ms=2.000000 p95_ms=2.000000 max_ms=2.000000 budget_ms=8 budget_pass=true"),
            "all state entries pass on the present endpoint"
        );
        assert!(report.contains("summary=hover_display n=100 successful=100 attempted=100 p50_ms=15.000000 p95_ms=15.000000 max_ms=15.000000"), "slow display does not fail the present gate");
        assert!(report.contains("summary=hover_send_input n=100 successful=100 attempted=100 p50_ms=12.000000 p95_ms=12.000000 max_ms=12.000000"));
        assert!(report.contains("send_input_qpc=90 send_input_to_present_ms=12.000000"));
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
                &directory,
                &injections
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
            !statistics(&mut report, "test", &mut ticks, 100, 1000, Some(8)).unwrap(),
            "95 ms misses the budget"
        );
        assert!(
            report.contains("p50_ms=50.000000 p95_ms=95.000000 max_ms=100.000000"),
            "nearest-rank statistics retain all samples"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_hover_budget_holds_at_eight_milliseconds_and_fails_one_tick_over() {
        let mut report = String::new();
        assert!(
            statistics(
                &mut report,
                "hover",
                &mut [8_000; 100],
                100,
                1_000_000,
                Some(8)
            )
            .unwrap()
        );
        assert!(
            !statistics(
                &mut report,
                "hover",
                &mut [8_001; 100],
                100,
                1_000_000,
                Some(8)
            )
            .unwrap()
        );
        assert!(report.contains("p95_ms=8.000000 max_ms=8.000000 budget_ms=8 budget_pass=true"));
        assert!(report.contains("p95_ms=8.001000 max_ms=8.001000 budget_ms=8 budget_pass=false"));
    }

    #[test]
    fn hover_gate_requires_a_fresh_native_entry_and_every_change_in_the_denominator() {
        let directory = directory();
        scenario(&directory);
        let observer = directory.join("observer.log");
        let log = fs::read_to_string(&observer).unwrap();
        let prefix = log.split("close commits=").next().unwrap();
        for (log, expected, status) in [
            (
                log.replacen("click_qpc=100", "click_qpc=0", 1),
                "summary=hover n=99 successful=99 attempted=100",
                "hover=1 panel_change=1 state_qpc=101 entry_qpc=0 send_input_qpc=90 send_input_to_present_ms=12.000000 status=MissingEntry",
            ),
            (
                log.replacen("click_qpc=120", "click_qpc=101", 1),
                "summary=hover n=99 successful=99 attempted=100",
                "hover=2 panel_change=2 state_qpc=121 entry_qpc=101 send_input_qpc=110 send_input_to_present_ms=12.000000 status=MissingEntry",
            ),
            (
                log.replacen("hover id=1 qpc=101", "hover id=1 qpc=102", 1),
                "summary=hover n=99 successful=99 attempted=100",
                "status=InvalidPanelClock",
            ),
            (
                finish(
                    format!("{prefix}hover id=101 qpc=2090 visible=true click_qpc=2085\n"),
                    101,
                    1,
                ),
                "summary=hover n=100 successful=100 attempted=101",
                "hover=101 panel_change=101 state_qpc=2090 entry_qpc=2085 status=MissingFrame",
            ),
        ] {
            fs::write(&observer, log).unwrap();
            let mut rows = crate::gate::present::read(&directory.join("present.csv"), 42).unwrap();
            let injections = (0..100).map(|index| 90 + index * 20).collect::<Vec<_>>();
            assert!(
                summarize_hover(&observer, 42, &mut rows, 10, &directory, &injections).is_err()
            );
            let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
            assert!(report.contains(status), "{status}");
            assert!(report.contains(expected), "{expected}");
            assert!(report.contains("gate=unmeasured_or_fail"));
            fs::remove_file(directory.join("hover-display.txt")).unwrap();
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hover_recovers_the_first_panel_present_only_as_a_derived_seed() {
        let directory = directory();
        scenario(&directory);
        let csv_path = directory.join("present.csv");
        let csv = fs::read_to_string(&csv_path).unwrap();
        let mut lines = csv.lines();
        let header = lines.next().unwrap();
        lines.next().unwrap();
        let first = lines.next().unwrap().replace(
            ",0,0,0,Composed: Flip,0,13,0,",
            ",0,20,0,Composed: Flip,0,13,20,",
        );
        fs::write(
            &csv_path,
            format!(
                "{header}\n{first}\n{}\n",
                lines.collect::<Vec<_>>().join("\n")
            ),
        )
        .unwrap();
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        assert_eq!(rows.rows().len(), 99, "no invented raw CSV row");
        summarize_hover(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
            &[],
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
        let first = report
            .lines()
            .find(|line| line.starts_with("hover=1 "))
            .unwrap();
        assert!(first.contains("source=Seed(SeedSource { csv_row: 2, present: Present { qpc: 122"));
        assert!(
            first
                .contains("between_presents_ms: Some(20.0), between_display_change_ms: Some(20.0)")
        );
        assert!(first.contains("present_qpc=102 status=Presented present_latency_ms=2.000000 display_status=Displayed displayed_qpc=115 display_latency_ms=15.000000"));
        assert!(report.contains("summary=hover n=100 successful=100 attempted=100"));
        assert!(report.contains("gate=pass"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_display_does_not_hide_valid_present_samples() {
        let directory = directory();
        scenario(&directory);
        let csv_path = directory.join("present.csv");
        let csv = fs::read_to_string(&csv_path).unwrap();
        fs::write(&csv_path, csv.replace(",13,", ",NA,")).unwrap();
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        summarize_hover(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
            &[],
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
        assert_eq!(
            report
                .matches("status=Presented present_latency_ms=2.000000")
                .count(),
            100
        );
        assert_eq!(report.matches("display_status=Unavailable displayed_qpc=unmeasured display_latency_ms=unmeasured").count(), 100);
        assert!(report.contains("summary=hover_display n=0 successful=0 attempted=100 p50_ms=unmeasured p95_ms=unmeasured max_ms=unmeasured"), "missing display is not a zero-latency sample");
        assert!(report.contains("gate=pass"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_present_joins_keep_samples_in_the_gate_denominator() {
        for status in ["Missing", "Ambiguous", "Reused", "Dropped"] {
            let directory = directory();
            scenario(&directory);
            let csv_path = directory.join("present.csv");
            let csv = fs::read_to_string(&csv_path).unwrap();
            let first = csv.lines().nth(1).unwrap();
            let csv = match status {
                "Missing" => csv.replacen(first, &first.replace(",102", ",109"), 1),
                "Ambiguous" => format!("{csv}{}\n", first.replace(",102", ",103")),
                "Dropped" => csv.replacen(first, &first.replace("DXGI,0,0,0,", "DXGI,0,0,1,"), 1),
                "Reused" => csv.clone(),
                _ => unreachable!("test enumerates invalid join statuses"),
            };
            fs::write(&csv_path, csv).unwrap();
            let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
            if status == "Reused" {
                rows.join_present(20, 101, 105, 1000).unwrap();
            }
            let result = summarize_hover(
                &directory.join("observer.log"),
                42,
                &mut rows,
                10,
                &directory,
                &[],
            );
            assert!(result.is_err(), "{status} cannot pass");
            let report = fs::read_to_string(directory.join("hover-display.txt")).unwrap();
            assert!(
                report.contains(&format!("status={status}")),
                "{status} remains explicit"
            );
            assert!(
                report.contains("summary=hover n=99 successful=99 attempted=100"),
                "{status} remains in the denominator"
            );
            assert!(report.contains("gate=unmeasured_or_fail"));
            fs::remove_dir_all(directory).unwrap();
        }
    }
}
