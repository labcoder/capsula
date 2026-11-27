//! Exact bounded frame/present joins with display latency reported separately.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};

use super::{Frame, Hover, Kind, Observation, load};
use crate::gate::present::{Join, MatchedPresent, PresentRows};
use crate::session::Heard;

/// One heard hold's present: QPC ticks after its recognition, after its key-down, and to its
/// display when `PresentMon` saw one, on a counter running at `frequency`; `cold` when it was its
/// launch's first hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gate) struct Presented {
    recognized: i64,
    key_down: i64,
    display: Option<i64>,
    frequency: i64,
    cold: bool,
}

/// Recognition to the first Listening dock present's submission, gated at p95 over all attempted
/// warm holds (owner, 2026-11-27: option A).
const WARM_BUDGET_MS: i64 = 40;
/// Recognition to the first Listening dock present's submission, gated on every attempted cold
/// hold (owner, 2026-11-27: option A).
const COLD_BUDGET_MS: i64 = 150;

/// Joins each of one batch's heard holds to the first dock frame painted while the line had the
/// Listening colour, from `listening on` until `listening off`, and that frame to the submission
/// of its present, whatever `PresentMon`'s `Dropped` flag says. Writes `listening-joins.txt` and
/// fails unless every one of `attempted` holds was presented.
pub(in crate::gate) fn join_listening(
    heard: &[Heard],
    attempted: usize,
    path: &Path,
    (pid, dock_chain): (u32, u64),
    rows: &mut PresentRows,
    output: &Path,
    presented: &mut Vec<Presented>,
) -> Result<()> {
    let (observation, validation) = load(path, pid);
    let frequency = observation.startup.map_or(0, |value| value.frequency);
    let mut report = String::from(
        "each hold joined to the first dock frame painted while the line showed Listening\n",
    );
    seed_report(&mut report, rows, frequency)?;
    if let Err(error) = &validation {
        writeln!(report, "observation_error={error:#}")?;
    }
    let before = presented.len();
    for (index, hold) in heard.iter().enumerate() {
        write!(
            report,
            "sample={} key_down_qpc={} recognized_qpc={} listening_qpc={} resting_qpc={} ",
            index + 1,
            hold.key_down,
            hold.recognized,
            hold.listening,
            hold.resting
        )?;
        let Some(frame) = observation.frames.iter().find(|frame| {
            frame.kind == Kind::Dock && (hold.listening..hold.resting).contains(&frame.paint_qpc)
        }) else {
            writeln!(report, "status=MissingFrame")?;
            continue;
        };
        if validation.is_err() {
            writeln!(report, "status=ObservationInvalid")?;
            continue;
        }
        let joined = rows.join_submission(
            dock_chain,
            frame.paint_qpc,
            frame.completion_qpc.unwrap_or(0),
            frequency,
        );
        if let Some((ticks, display)) =
            sample(&mut report, frame, hold.recognized, joined, frequency)?
        {
            presented.push(Presented {
                recognized: ticks,
                key_down: ticks + (hold.recognized - hold.key_down),
                display,
                frequency,
                cold: index == 0,
            });
        }
    }
    let joined = presented.len() - before;
    writeln!(
        report,
        "attempted={attempted} heard={} presented={joined}",
        heard.len()
    )?;
    save_report(&output.join("listening-joins.txt"), &report)?;
    ensure!(
        validation.is_ok() && joined == attempted,
        "a Listening hold went unheard or unpresented; see {}",
        output.display()
    );
    Ok(())
}

/// The Listening row over `batches` launches of `batch_holds` holds (owner, 2026-11-27: option
/// A): recognition to the submission of the line's first Listening dock present. Cold holds, each
/// launch's first, are gated every one at [`COLD_BUDGET_MS`]; warm holds, the rest, at p95
/// [`WARM_BUDGET_MS`] over all attempted. Key-down to present and recognition to display are
/// reported beside them, not gated; display is known only for presents `PresentMon` did not mark
/// dropped, so each group reports how many of its holds were displayed.
pub(in crate::gate) fn summarize_listening(
    presented: &[Presented],
    batches: usize,
    batch_holds: usize,
    output: &Path,
) -> Result<()> {
    let frequency = presented.first().map_or(0, |sample| sample.frequency);
    ensure!(
        presented.iter().all(|sample| sample.frequency == frequency),
        "Listening batches ran on different counter frequencies"
    );
    let verdict = |pass: bool| if pass { "pass" } else { "unmeasured_or_fail" };
    let mut report = String::from(
        "hold recognised to the submission of the line's first Listening dock present, gated: cold (each launch's first hold) every one, warm (the rest) at p95 over all attempted; key-down to that present and display latency reported beside it, not gated\n",
    );
    let mut pass = true;
    for (label, cold, attempted) in [
        ("cold", true, batches),
        ("warm", false, batches * batch_holds.saturating_sub(1)),
    ] {
        let group = presented
            .iter()
            .filter(|sample| sample.cold == cold)
            .collect::<Vec<_>>();
        let mut recognized = group
            .iter()
            .map(|sample| sample.recognized)
            .collect::<Vec<_>>();
        let mut key_down = group
            .iter()
            .map(|sample| sample.key_down)
            .collect::<Vec<_>>();
        let mut displays = group
            .iter()
            .filter_map(|sample| sample.display)
            .collect::<Vec<_>>();
        let displayed = displays.len();
        let gated = format!("{label}_recognized_to_listening");
        let budget_pass = if cold {
            let measured = statistics(
                &mut report,
                &gated,
                &mut recognized,
                attempted,
                frequency,
                None,
            )?;
            let every = measured
                && recognized
                    .iter()
                    .all(|&ticks| within(ticks, frequency, COLD_BUDGET_MS));
            writeln!(
                report,
                "cold_every_hold_budget_ms={COLD_BUDGET_MS} budget_pass={every}"
            )?;
            every
        } else {
            statistics(
                &mut report,
                &gated,
                &mut recognized,
                attempted,
                frequency,
                Some(WARM_BUDGET_MS),
            )?
        };
        statistics(
            &mut report,
            &format!("{label}_key_down_to_listening"),
            &mut key_down,
            attempted,
            frequency,
            None,
        )?;
        statistics(
            &mut report,
            &format!("{gated}_display"),
            &mut displays,
            attempted,
            frequency,
            None,
        )?;
        let group_pass = group.len() == attempted && budget_pass;
        writeln!(
            report,
            "{label}_holds attempted={attempted} presented={} displayed={displayed} gate={}",
            group.len(),
            verdict(group_pass)
        )?;
        pass &= group_pass;
    }
    writeln!(
        report,
        "expected_holds={} presented={} gate={}",
        batches * batch_holds,
        presented.len(),
        verdict(pass)
    )?;
    save_report(&output.join("listening-display.txt"), &report)?;
    ensure!(
        pass,
        "Listening present gate incomplete or over budget; see {}",
        output.display()
    );
    Ok(())
}

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

pub(in crate::gate) fn milliseconds(ticks: i64, frequency: i64) -> Result<String> {
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
    let pass = budget_ms.is_none_or(|budget| within(p95, frequency, budget));
    if let Some(budget) = budget_ms {
        write!(report, " budget_ms={budget} budget_pass={pass}")?;
    }
    writeln!(report)?;
    Ok(pass)
}

/// Whether `ticks` on a counter running at `frequency` is no longer than `budget_ms`.
fn within(ticks: i64, frequency: i64, budget_ms: i64) -> bool {
    i128::from(ticks) * 1000 <= i128::from(frequency) * i128::from(budget_ms)
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

    /// Four heard holds, recognised at 1000, 2000, 3000 and 4000 ms, the line Listening from 1 ms
    /// to 100 ms later. Each has an earlier dock frame that must not count, then the Listening
    /// frame, whose present comes `delays` ms after recognition.
    fn listening_scenario(directory: &Path, delays: [i64; 4]) -> Vec<Heard> {
        let mut text = format!(
            "{GEOMETRY}{COMMIT}{STARTUP}paint frame=1 qpc=12 kind=panel hover=0 drawn_epoch=0 drawn_geometry=0 mask_epoch=1\n"
        );
        let mut csv = CSV_HEADER.to_owned();
        let mut heard = Vec::new();
        let mut frame = 1;
        for (index, delay) in (1..=4).zip(delays) {
            let recognized = 1000 * index;
            for (paint, present) in [
                (recognized - 50, recognized - 49),
                (recognized + 2, recognized + delay),
            ] {
                frame += 1;
                writeln!(text, "paint frame={frame} qpc={paint} kind=dock hover=0 drawn_epoch=1 drawn_geometry=1 mask_epoch=1\ndrawn frame={frame} qpc={}", paint + 6).unwrap();
                writeln!(
                    csv,
                    "app.exe,42,0xA,DXGI,0,0,0,0.1,0,0,0,Composed: Flip,0,13,0,0,0,0,{present}"
                )
                .unwrap();
            }
            heard.push(Heard {
                key_down: recognized - 300,
                recognized,
                listening: recognized + 1,
                resting: recognized + 100,
            });
        }
        fs::write(directory.join("observer.log"), finish(text, frame, 1)).unwrap();
        fs::write(directory.join("present.csv"), csv).unwrap();
        heard
    }

    /// Joins `heard` against the scenario, then summarises the presents as `batches` launches of
    /// `batch_holds` holds.
    fn listening_report(
        directory: &Path,
        heard: &[Heard],
        (batches, batch_holds): (usize, usize),
    ) -> (bool, Vec<Presented>, String) {
        let mut rows = crate::gate::present::read(&directory.join("present.csv"), 42).unwrap();
        let mut presented = Vec::new();
        let joined = join_listening(
            heard,
            heard.len(),
            &directory.join("observer.log"),
            (42, 10),
            &mut rows,
            directory,
            &mut presented,
        );
        let joins = directory.join("listening-joins.txt");
        let mut report = fs::read_to_string(&joins).unwrap();
        fs::remove_file(joins).unwrap();
        let summarized = summarize_listening(&presented, batches, batch_holds, directory);
        let summary = directory.join("listening-display.txt");
        report.push_str(&fs::read_to_string(&summary).unwrap());
        fs::remove_file(summary).unwrap();
        (joined.is_ok() && summarized.is_ok(), presented, report)
    }

    #[test]
    fn listening_times_the_first_dock_frame_while_listening_from_recognition() {
        let directory = directory();
        let mut heard = listening_scenario(&directory, [3, 5, 7, 6]);
        let (passed, presented, report) = listening_report(&directory, &heard, (1, 4));
        assert!(passed, "{report}");
        assert_eq!(
            presented
                .iter()
                .map(|sample| sample.cold)
                .collect::<Vec<_>>(),
            [true, false, false, false],
            "the launch's first hold is cold and the rest are warm"
        );
        assert!(report.contains("summary=cold_recognized_to_listening n=1 successful=1 attempted=1 p50_ms=3.000000 p95_ms=3.000000 max_ms=3.000000\ncold_every_hold_budget_ms=150 budget_pass=true"), "{report}");
        assert!(report.contains("summary=warm_recognized_to_listening n=3 successful=3 attempted=3 p50_ms=6.000000 p95_ms=7.000000 max_ms=7.000000 budget_ms=40 budget_pass=true"), "{report}");
        assert!(report.contains("summary=warm_key_down_to_listening n=3 successful=3 attempted=3 p50_ms=306.000000 p95_ms=307.000000 max_ms=307.000000"), "{report}");
        assert!(report.contains("expected_holds=4 presented=4 gate=pass"));
        let (_, second, _) = listening_report(&directory, &heard, (1, 4));
        let (passed, report) = summary(&[presented, second].concat(), 2, 4);
        assert!(passed, "{report}");
        assert!(
            report.contains("cold_holds attempted=2 presented=2 displayed=2 gate=pass")
                && report.contains("warm_holds attempted=6 presented=6 displayed=6 gate=pass"),
            "each launch's first hold is cold: {report}"
        );
        let (passed, _, report) = listening_report(&directory, &heard, (1, 5));
        assert!(
            !passed,
            "a hold that never reached Listening stays in the denominator"
        );
        assert!(
            report.contains("warm_holds attempted=4 presented=3")
                && report.contains("gate=unmeasured_or_fail")
        );
        heard[0].resting = heard[0].listening + 1;
        let (passed, presented, report) = listening_report(&directory, &heard, (1, 4));
        assert!(
            !passed,
            "a frame painted after Listening ended is not Listening"
        );
        assert!(
            report.contains("status=MissingFrame") && report.contains("presented=3"),
            "{report}"
        );
        assert_eq!(presented.len(), 3);
        assert!(
            report.contains("cold_holds attempted=1 presented=0"),
            "the second hold never becomes the launch's cold one: {report}"
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_dropped_listening_present_counts_at_its_submission_without_a_display() {
        let directory = directory();
        let heard = listening_scenario(&directory, [3, 5, 7, 6]);
        let csv_path = directory.join("present.csv");
        let csv = fs::read_to_string(&csv_path).unwrap();
        let shown = "app.exe,42,0xA,DXGI,0,0,0,0.1,0,0,0,Composed: Flip,0,13,0,0,0,0,2005\n";
        assert!(csv.contains(shown));
        fs::write(
            &csv_path,
            csv.replace(shown, &shown.replace("DXGI,0,0,0,", "DXGI,0,0,1,")),
        )
        .unwrap();
        let (passed, presented, report) = listening_report(&directory, &heard, (1, 4));
        assert!(passed, "{report}");
        assert_eq!(
            presented[1].display, None,
            "a dropped present proves no display"
        );
        assert!(report.contains("present_qpc=2005 status=Presented present_latency_ms=5.000000 display_status=Unavailable"), "{report}");
        assert!(report.contains("summary=warm_recognized_to_listening_display n=2 successful=2 attempted=3 p50_ms=19.000000 p95_ms=20.000000 max_ms=20.000000"), "{report}");
        assert!(
            report.contains("cold_holds attempted=1 presented=1 displayed=1 gate=pass")
                && report.contains("warm_holds attempted=3 presented=3 displayed=2 gate=pass"),
            "{report}"
        );
    }

    /// A warm hold of `recognized` µs, displayed `display` µs after recognition when known.
    fn warm(recognized: i64, display: Option<i64>) -> Presented {
        Presented {
            recognized,
            key_down: recognized + 300_000,
            display,
            frequency: 1_000_000,
            cold: false,
        }
    }

    /// A cold hold of `recognized` µs with no known display.
    fn cold(recognized: i64) -> Presented {
        Presented {
            cold: true,
            ..warm(recognized, None)
        }
    }

    /// Summarises `presented` as `batches` launches of `batch_holds` holds in a fresh folder.
    fn summary(presented: &[Presented], batches: usize, batch_holds: usize) -> (bool, String) {
        let directory = directory();
        let passed = summarize_listening(presented, batches, batch_holds, &directory).is_ok();
        let report = fs::read_to_string(directory.join("listening-display.txt")).unwrap();
        (passed, report)
    }

    #[test]
    fn warm_holds_pass_at_a_40_ms_p95_and_fail_one_tick_over() {
        let holds = |at: usize, over: usize, ticks: i64| {
            [
                [cold(1_000); 10].as_slice(),
                &[warm(40_000, Some(50_000)); 90][..at],
                vec![warm(ticks, None); over].as_slice(),
            ]
            .concat()
        };
        let (passed, report) = summary(&holds(86, 4, 1_000_000), 10, 10);
        assert!(passed, "the four slowest of 90 lie above the p95: {report}");
        assert!(report.contains("summary=warm_recognized_to_listening n=90 successful=90 attempted=90 p50_ms=40.000000 p95_ms=40.000000 max_ms=1000.000000 budget_ms=40 budget_pass=true"), "{report}");
        let (passed, report) = summary(&holds(85, 5, 40_001), 10, 10);
        assert!(!passed, "{report}");
        assert!(
            report.contains("p95_ms=40.001000 max_ms=40.001000 budget_ms=40 budget_pass=false"),
            "{report}"
        );
        assert!(
            report.contains(
                "warm_holds attempted=90 presented=90 displayed=85 gate=unmeasured_or_fail"
            ),
            "{report}"
        );
    }

    #[test]
    fn every_cold_hold_passes_at_150_ms_and_one_tick_over_fails() {
        let rest = [warm(1_000, None); 20];
        let (passed, report) = summary(&[[cold(150_000); 20], rest].concat(), 20, 2);
        assert!(passed, "{report}");
        assert!(report.contains("summary=cold_recognized_to_listening n=20 successful=20 attempted=20 p50_ms=150.000000 p95_ms=150.000000 max_ms=150.000000\ncold_every_hold_budget_ms=150 budget_pass=true"), "{report}");
        let mut first = [cold(1_000); 20];
        first[7] = cold(150_001);
        let (passed, report) = summary(&[first, rest].concat(), 20, 2);
        assert!(
            !passed,
            "one slow cold hold fails though the p95 passes: {report}"
        );
        assert!(
            report.contains(
                "p95_ms=1.000000 max_ms=150.001000\ncold_every_hold_budget_ms=150 budget_pass=false"
            ),
            "{report}"
        );
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
        }
    }
}
