//! Exact bounded frame/present joins with display latency reported separately.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};

use super::{Frame, Kind, Observation, load};
use crate::gate::present::{Join, MatchedPresent, PresentRows};

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
    let mut report = String::from(
        "keyboard WM_INPUT QPC-to-present gate; display latency reported separately, not gated\n",
    );
    seed_report(&mut report, rows, frequency)?;
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let mut chords = Vec::new();
    let mut displays = Vec::new();
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
                rows.join_present(
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
        if let Some((ticks, display)) = ticks {
            successful += 1;
            if index < 100 {
                chords.push(ticks);
                displays.extend(display);
            }
        }
    }
    let budget_pass = statistics(
        &mut report,
        "chords",
        &mut chords,
        canvas_pairs.len().min(100),
        frequency,
        Some(7),
    )?;
    statistics(
        &mut report,
        "chords_display",
        &mut displays,
        canvas_pairs.len().min(100),
        frequency,
        None,
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
        "keyboard present gate incomplete or over budget; see {}",
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

fn clicked_panel_frame(
    observation: &Observation,
    click_qpc: i64,
) -> std::result::Result<&Frame, &'static str> {
    let mut shows = observation
        .hovers
        .iter()
        .filter(|change| change.visible && change.click_qpc == Some(click_qpc));
    let change = shows.next().ok_or("MissingClickPanelEntry")?;
    if shows.next().is_some() {
        return Err("AmbiguousClickPanelEntry");
    }
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

pub(in crate::gate) fn summarize_click(
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
        "line button-down dispatch QPC-to-present gate; excludes OS routing and UI-thread queue wait; SendInput-to-present and display reported separately, not gated\n",
    );
    seed_report(&mut report, rows, frequency)?;
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let mut pinned = None;
    let mut latencies = Vec::new();
    let mut displays = Vec::new();
    let mut injection_latencies = Vec::new();
    let mut attempted = 0;
    for click in &observation.clicks {
        attempted += 1;
        write!(report, "click={attempted} start_qpc={} ", click.qpc)?;
        let frame = match clicked_panel_frame(&observation, click.qpc) {
            Ok(frame) => frame,
            Err(status) => {
                writeln!(report, "status={status}")?;
                continue;
            }
        };
        write!(report, "panel_change={} ", frame.hover)?;
        if validation.is_err() {
            writeln!(report, "status=ObservationInvalid")?;
            continue;
        }
        let Some(end) = frame.completion_qpc else {
            writeln!(report, "status=MissingCompletion frame={}", frame.id)?;
            continue;
        };
        let chain = match rows.non_dock_swapchain(dock_chain, frame.paint_qpc, end, frequency) {
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
        let joined = rows.join_present(chain, frame.paint_qpc, end, frequency);
        let previous_click = observation.clicks[..attempted - 1]
            .last()
            .map_or(0, |previous| previous.qpc);
        injection_latencies.extend(injection_sample(
            &mut report,
            previous_click,
            click.qpc,
            injections,
            &joined,
            frequency,
        )?);
        if let Some((ticks, display)) = sample(&mut report, frame, click.qpc, joined, frequency)? {
            latencies.push(ticks);
            displays.extend(display);
        }
    }
    let budget_pass = statistics(
        &mut report,
        "click",
        &mut latencies,
        attempted,
        frequency,
        Some(8),
    )?;
    for (label, ticks) in [
        ("click_display", &mut displays),
        ("click_send_input", &mut injection_latencies),
    ] {
        statistics(&mut report, label, ticks, attempted, frequency, None)?;
    }
    let shown = observation
        .hovers
        .iter()
        .filter(|change| change.visible)
        .count();
    let pass = validation.is_ok()
        && attempted == 100
        && shown == 100
        && latencies.len() == 100
        && budget_pass;
    writeln!(
        report,
        "expected_clicks=100 shown={shown} panel_swapchain={pinned:?} gate={}",
        if pass { "pass" } else { "unmeasured_or_fail" }
    )?;
    save_report(&output.join("click-display.txt"), &report)?;
    ensure!(
        pass,
        "click present gate incomplete or over budget; see {}",
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
            }
            let (kind, epoch, geometry, hover_id, chain) = if hover {
                ("panel", 0, 0, index + 1, 20)
            } else {
                ("dock", 1, 1, 0, 10)
            };
            writeln!(text, "paint frame={} qpc={paint} kind={kind} hover={hover_id} drawn_epoch={epoch} drawn_geometry={geometry} mask_epoch=1\ndrawn frame={} qpc={}", index + 2, index + 2, paint + 4).unwrap();
            let display_ms = if !hover && index == 100 { 50 } else { 13 };
            writeln!(
                csv,
                "app.exe,42,0x{chain:X},DXGI,0,0,0,0.1,0,0,0,Composed: Flip,0,{display_ms},0,0,0,0,{}",
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
            report.contains("display_latency_ms=52.000000"),
            "slow hold is retained separately"
        );
        assert!(report.contains("summary=chords n=100 successful=100 attempted=100 p50_ms=2.000000 p95_ms=2.000000 max_ms=2.000000 budget_ms=7 budget_pass=true"), "present gate ignores the slower display endpoint");
        assert!(report.contains("summary=chords_display n=100 successful=100 attempted=100 p50_ms=15.000000 p95_ms=15.000000 max_ms=15.000000"), "display statistics stay beside the present gate");
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
    fn click_pins_only_unique_panel_chain_and_uses_nearest_rank_percentiles() {
        let directory = directory();
        let injections = scenario(&directory, Kind::Panel)
            .iter()
            .map(|(start, _)| start - 10)
            .collect::<Vec<_>>();
        let csv_path = directory.join("present.csv");
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        summarize_click(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
            &injections,
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("click-display.txt")).unwrap();
        assert!(
            report.contains("summary=click n=100 successful=100 attempted=100 p50_ms=2.000000 p95_ms=2.000000 max_ms=2.000000 budget_ms=8 budget_pass=true"),
            "all state entries pass on the present endpoint"
        );
        assert!(report.contains("summary=click_display n=100 successful=100 attempted=100 p50_ms=15.000000 p95_ms=15.000000 max_ms=15.000000"), "slow display does not fail the present gate");
        assert!(report.contains("summary=click_send_input n=100 successful=100 attempted=100 p50_ms=12.000000 p95_ms=12.000000 max_ms=12.000000"));
        assert!(report.contains("send_input_qpc=90 send_input_to_present_ms=12.000000"));
        fs::remove_file(directory.join("click-display.txt")).unwrap();
        let csv = fs::read_to_string(&csv_path).unwrap();
        fs::write(&csv_path, csv.replacen("0x14", "0x15", 1)).unwrap();
        let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
        assert!(
            summarize_click(
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
            fs::read_to_string(directory.join("click-display.txt"))
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
    fn click_gate_requires_native_button_down_origin_and_every_click_in_the_denominator() {
        let directory = directory();
        scenario(&directory, Kind::Panel);
        let observer = directory.join("observer.log");
        let log = fs::read_to_string(&observer).unwrap();
        let prefix = log.split("close commits=").next().unwrap();
        let extra = finish(
            format!("{prefix}click qpc=2090 point=2,2 mask_epoch=1 marker=0x43505331\n"),
            101,
            1,
        );
        for (log, expected, status) in [
            (
                log.replacen("click_qpc=100", "click_qpc=101", 1),
                "summary=click n=99 successful=99 attempted=100",
                "status=MissingClickPanelEntry",
            ),
            (
                log.replacen("click_qpc=100", "click_qpc=0", 1),
                "summary=click n=99 successful=99 attempted=100",
                "status=MissingClickPanelEntry",
            ),
            (
                log.replacen("hover id=1 qpc=101", "hover id=1 qpc=102", 1),
                "summary=click n=99 successful=99 attempted=100",
                "status=InvalidPanelClock",
            ),
            (
                extra,
                "summary=click n=100 successful=100 attempted=101",
                "status=MissingClickPanelEntry",
            ),
            (
                finish(
                    format!("{prefix}hover id=101 qpc=2090 visible=true click_qpc=0\n"),
                    101,
                    1,
                ),
                "summary=click n=100 successful=100 attempted=100",
                "shown=101",
            ),
            (
                finish(
                    format!("{prefix}hover id=101 qpc=2090 visible=true click_qpc=100\n"),
                    101,
                    1,
                ),
                "summary=click n=99 successful=99 attempted=100",
                "status=AmbiguousClickPanelEntry",
            ),
        ] {
            fs::write(&observer, log).unwrap();
            let mut rows = crate::gate::present::read(&directory.join("present.csv"), 42).unwrap();
            assert!(summarize_click(&observer, 42, &mut rows, 10, &directory, &[]).is_err());
            let report = fs::read_to_string(directory.join("click-display.txt")).unwrap();
            assert!(report.contains(status));
            assert!(report.contains(expected));
            assert!(report.contains("gate=unmeasured_or_fail"));
            fs::remove_file(directory.join("click-display.txt")).unwrap();
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn click_recovers_the_first_panel_present_only_as_a_derived_seed() {
        let directory = directory();
        scenario(&directory, Kind::Panel);
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
        summarize_click(
            &directory.join("observer.log"),
            42,
            &mut rows,
            10,
            &directory,
            &[],
        )
        .unwrap();
        let report = fs::read_to_string(directory.join("click-display.txt")).unwrap();
        let first = report
            .lines()
            .find(|line| line.starts_with("click=1 "))
            .unwrap();
        assert!(first.contains("source=Seed(SeedSource { csv_row: 2, present: Present { qpc: 122"));
        assert!(
            first
                .contains("between_presents_ms: Some(20.0), between_display_change_ms: Some(20.0)")
        );
        assert!(first.contains("present_qpc=102 status=Presented present_latency_ms=2.000000 display_status=Displayed displayed_qpc=115 display_latency_ms=15.000000"));
        assert!(report.contains("summary=click n=100 successful=100 attempted=100"));
        assert!(report.contains("gate=pass"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_display_does_not_hide_valid_present_samples() {
        for kind in [Kind::Dock, Kind::Panel] {
            let directory = directory();
            let pairs = scenario(&directory, kind);
            let csv_path = directory.join("present.csv");
            let csv = fs::read_to_string(&csv_path).unwrap();
            fs::write(
                &csv_path,
                csv.replace(",13,", ",NA,").replace(",50,", ",0,"),
            )
            .unwrap();
            let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
            let (file, label, count) = match kind {
                Kind::Dock => {
                    summarize_keyboard(
                        &pairs,
                        &directory.join("observer.log"),
                        42,
                        &mut rows,
                        10,
                        &directory,
                    )
                    .unwrap();
                    ("keyboard-display.txt", "chords_display", 101)
                }
                Kind::Panel => {
                    summarize_click(
                        &directory.join("observer.log"),
                        42,
                        &mut rows,
                        10,
                        &directory,
                        &[],
                    )
                    .unwrap();
                    ("click-display.txt", "click_display", 100)
                }
            };
            let report = fs::read_to_string(directory.join(file)).unwrap();
            assert_eq!(
                report
                    .matches("status=Presented present_latency_ms=2.000000")
                    .count(),
                count
            );
            assert_eq!(report.matches("display_status=Unavailable displayed_qpc=unmeasured display_latency_ms=unmeasured").count(), count);
            assert!(report.contains(&format!("summary={label} n=0 successful=0 attempted=100 p50_ms=unmeasured p95_ms=unmeasured max_ms=unmeasured")), "missing display is not a zero-latency sample");
            assert!(report.contains("gate=pass"));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn invalid_present_joins_keep_samples_in_the_gate_denominator() {
        for kind in [Kind::Dock, Kind::Panel] {
            for status in ["Missing", "Ambiguous", "Reused", "Dropped"] {
                let directory = directory();
                let pairs = scenario(&directory, kind);
                let csv_path = directory.join("present.csv");
                let csv = fs::read_to_string(&csv_path).unwrap();
                let first = csv.lines().nth(1).unwrap();
                let csv = match status {
                    "Missing" => csv.replacen(first, &first.replace(",102", ",109"), 1),
                    "Ambiguous" => format!("{csv}{}\n", first.replace(",102", ",103")),
                    "Dropped" => {
                        csv.replacen(first, &first.replace("DXGI,0,0,0,", "DXGI,0,0,1,"), 1)
                    }
                    "Reused" => csv.clone(),
                    _ => unreachable!("test enumerates invalid join statuses"),
                };
                fs::write(&csv_path, csv).unwrap();
                let mut rows = crate::gate::present::read(&csv_path, 42).unwrap();
                if status == "Reused" {
                    let chain = if kind == Kind::Dock { 10 } else { 20 };
                    rows.join_present(chain, 101, 105, 1000).unwrap();
                }
                let (result, file, expected) = match kind {
                    Kind::Dock => (
                        summarize_keyboard(
                            &pairs,
                            &directory.join("observer.log"),
                            42,
                            &mut rows,
                            10,
                            &directory,
                        ),
                        "keyboard-display.txt",
                        "total_successful=100 attempted=101",
                    ),
                    Kind::Panel => (
                        summarize_click(
                            &directory.join("observer.log"),
                            42,
                            &mut rows,
                            10,
                            &directory,
                            &[],
                        ),
                        "click-display.txt",
                        "summary=click n=99 successful=99 attempted=100",
                    ),
                };
                assert!(result.is_err(), "{kind:?} {status} cannot pass");
                let report = fs::read_to_string(directory.join(file)).unwrap();
                assert!(
                    report.contains(&format!("status={status}")),
                    "{kind:?} {status} remains explicit"
                );
                assert!(
                    report.contains(expected),
                    "{kind:?} {status} remains in the denominator"
                );
                assert!(report.contains("gate=unmeasured_or_fail"));
                fs::remove_dir_all(directory).unwrap();
            }
        }
    }
}
