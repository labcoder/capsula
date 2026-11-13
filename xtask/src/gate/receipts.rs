//! Tagged clicks land only on our proxy or covering EDIT; actual receipts decide who got them.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use windows::Win32::Foundation::{POINT, RECT};

use crate::platform::{
    gate_capture,
    input_session::{Session, Target},
};
use crate::session::Counts;

use super::observation::{self, Commit, Frame, Geometry, Kind, Observation, Reader};
use super::present::{Join, PresentRows};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Rest,
    /// Unused until C2a's pull brings the motion trial back.
    Motion,
}

impl Mode {
    fn categories(self) -> &'static [&'static str] {
        match self {
            Self::Rest => &["center", "edge", "outside", "external"],
            Self::Motion => &["body", "previous", "outside", "external", "retract"],
        }
    }
}

const HEADER: &str = "sequence,category,x,y,move_begin,move_end,move_accepted,click_begin,click_end,click_accepted,recipient,receipt_qpc,epoch,recipient_count,status";

struct Received {
    recipient: &'static str,
    qpc: i64,
    epoch: u64,
    count: usize,
}

#[derive(Debug, Clone, Copy)]
struct Displayed {
    at: i64,
    painted: i64,
    geometry: u64,
    present: i64,
    frame: u64,
}

pub(super) fn exercise(
    session: &mut Session,
    observer: &Path,
    output: &Path,
    counts: &mut Counts,
    mode: Mode,
) -> Result<()> {
    let mut reader = Reader::open(observer, session.product()?.pid)?;
    let data = reader.update()?;
    let startup = data
        .startup
        .as_ref()
        .context("observer startup missing before receipts")?;
    let cover = cover_rect(startup.overlay, startup.work_area)?;
    let dock = startup.dock;
    session.cover(cover)?;
    let result = (|| -> Result<()> {
        let mut file = File::create_new(output.join("receipt-trials.csv"))?;
        writeln!(file, "{HEADER}")?;
        let duration = Duration::from_secs(if mode == Mode::Rest { 20 } else { 30 });
        let end = Instant::now() + duration;
        let categories = mode.categories();
        let mut sequence = 0usize;
        let mut seen = BTreeMap::<&str, usize>::new();
        while Instant::now() < end {
            session.guard()?;
            ensure!(
                session.fixture_foreground(),
                "receipt click changed fixture foreground"
            );
            let category = categories[sequence % categories.len()];
            sequence += 1;
            let data = reader.update()?;
            let commit = data.commits.last().context("no accepted observer mask")?;
            let now = gate_capture::qpc()?;
            ensure!(now >= commit.qpc, "observer mask has a future timestamp");
            let point = candidate(data, category, dock, cover);
            let Some(point) = point else {
                writeln!(
                    file,
                    "{sequence},{category},0,0,0,0,0,0,0,0,none,0,0,0,no_candidate"
                )?;
                session.wait(Duration::from_millis(250))?;
                continue;
            };
            click_once(
                session,
                &mut reader,
                counts,
                &mut file,
                sequence,
                category,
                point,
            )?;
            *seen.entry(category).or_default() += 1;
            session.wait(Duration::from_millis(250))?;
        }
        for category in categories {
            ensure!(
                seen.get(category).copied().unwrap_or(0) > 0,
                "no actual {category} receipt in bounded window"
            );
        }
        Ok(())
    })();
    let uncovered = session.uncover();
    crate::session::combine([result, uncovered])
}

fn click_once(
    session: &mut Session,
    reader: &mut Reader,
    counts: &mut Counts,
    file: &mut File,
    sequence: usize,
    category: &str,
    point: [i32; 2],
) -> Result<()> {
    counts.attempts += 1;
    let native_point = POINT {
        x: point[0],
        y: point[1],
    };
    let moved = session.move_pointer(native_point, Target::Receipt);
    let move_begin = session.last_qpc.unwrap_or(0);
    let move_end = session.last_end_qpc.unwrap_or(0);
    let move_accepted = session.last_accepted;
    if let Err(error) = moved {
        writeln!(
            file,
            "{sequence},{category},{},{},{move_begin},{move_end},{move_accepted},0,0,0,none,0,0,0,failed",
            point[0], point[1]
        )?;
        counts.behavior_fail += 1;
        return Err(error);
    }
    let prepared = (|| -> Result<(usize, usize)> {
        session.wait(Duration::from_millis(2))?;
        Ok((
            session.fixture_receipt()?.count,
            reader.update()?.clicks.len(),
        ))
    })();
    let (edit_before, proxy_before) = match prepared {
        Ok(value) => value,
        Err(error) => {
            writeln!(
                file,
                "{sequence},{category},{},{},{move_begin},{move_end},{move_accepted},0,0,0,none,0,0,0,failed",
                point[0], point[1]
            )?;
            counts.behavior_fail += 1;
            return Err(error);
        }
    };
    let clicked = session.click(native_point, Target::Receipt);
    let click_begin = session.last_qpc.unwrap_or(0);
    let click_end = session.last_end_qpc.unwrap_or(0);
    let click_accepted = session.last_accepted;
    if let Err(error) = clicked {
        writeln!(
            file,
            "{sequence},{category},{},{},{move_begin},{move_end},{move_accepted},{click_begin},{click_end},{click_accepted},none,0,0,0,failed",
            point[0], point[1]
        )?;
        counts.behavior_fail += 1;
        return Err(error);
    }
    counts.sent += 1;
    let received = receive_click(session, reader, point, edit_before, proxy_before);
    let Received {
        recipient,
        qpc: receipt_qpc,
        epoch,
        count: receipt_count,
    } = match received {
        Ok(receipt) => receipt,
        Err(error) => {
            writeln!(
                file,
                "{sequence},{category},{},{},{move_begin},{move_end},{move_accepted},{click_begin},{click_end},{click_accepted},none,0,0,0,failed",
                point[0], point[1]
            )?;
            counts.behavior_fail += 1;
            return Err(error);
        }
    };
    writeln!(
        file,
        "{sequence},{category},{},{},{move_begin},{move_end},{move_accepted},{click_begin},{click_end},{click_accepted},{recipient},{receipt_qpc},{epoch},{receipt_count},ok",
        point[0], point[1]
    )?;
    counts.behavior_pass += 1;

    Ok(())
}

fn receive_click(
    session: &Session,
    reader: &mut Reader,
    point: [i32; 2],
    edit_before: usize,
    proxy_before: usize,
) -> Result<Received> {
    let acknowledgement = Instant::now() + Duration::from_millis(250);
    loop {
        session.guard()?;
        ensure!(session.fixture_foreground(), "receipt changed foreground");
        let edit = session.fixture_receipt()?;
        let data = reader.update()?;
        let edit_count = edit
            .count
            .checked_sub(edit_before)
            .context("EDIT counter went backwards")?;
        let proxy_count = data
            .clicks
            .len()
            .checked_sub(proxy_before)
            .context("proxy counter went backwards")?;
        ensure!(
            edit_count + proxy_count <= 1,
            "click produced multiple recipient receipts"
        );
        if edit_count == 1 {
            ensure!(
                [edit.point.x, edit.point.y] == point,
                "EDIT received a different pixel"
            );
            break Ok(Received {
                recipient: "edit",
                qpc: edit.qpc,
                epoch: 0,
                count: edit.count,
            });
        }
        if proxy_count == 1 {
            let click = data.clicks.last().context("proxy receipt missing")?;
            ensure!(click.point == point, "proxy received a different pixel");
            break Ok(Received {
                recipient: "proxy",
                qpc: click.qpc,
                epoch: click.epoch,
                count: data.clicks.len(),
            });
        }
        ensure!(
            Instant::now() < acknowledgement,
            "actual recipient acknowledgement missing"
        );
        session.wait(Duration::from_millis(1))?;
    }
}

fn cover_rect(overlay: [i32; 4], work: [i32; 4]) -> Result<RECT> {
    let [x, y, width, height] = overlay;
    let [left, top, work_width, work_height] = work;
    let rect = RECT {
        left: (x - 8).max(left),
        top: (y - 8).max(top),
        right: (x + width + 8).min(left + work_width),
        bottom: (y + height + 8).min(top + work_height),
    };
    ensure!(
        rect.right > rect.left
            && rect.bottom > rect.top
            && rect.right - rect.left <= 1024
            && rect.bottom - rect.top <= 1024,
        "receipt cover is invalid"
    );
    Ok(rect)
}

fn inside(point: [i32; 2], rect: [i32; 4]) -> bool {
    point[0] >= rect[0]
        && point[1] >= rect[1]
        && point[0] < rect[0] + rect[2]
        && point[1] < rect[1] + rect[3]
}

fn candidate(data: &Observation, category: &str, dock: [i32; 4], cover: RECT) -> Option<[i32; 2]> {
    let panel = data.startup.as_ref()?.panel;
    let commit = data.commits.last()?;
    let geometry = data.geometries.get(&commit.geometry)?;
    let y = dock[1] + dock[3] / 2;
    let row = geometry
        .rows
        .get(usize::try_from(y - geometry.origin_y).ok()?)?;
    let [left, right] = row.drawn[0]?;
    let point = match category {
        "center" => Some([left.midpoint(right), y]),
        "edge" => Some([left, y]),
        "body" => (right > dock[0] + dock[2]).then_some([right - 1, y]),
        "previous" => row.owned.iter().flatten().find_map(|span| {
            [span[0], span[1] - 1]
                .into_iter()
                .find(|x| !geometry.contains_drawn([*x, y]))
                .map(|x| [x, y])
        }),
        "outside" => {
            let middle = left.midpoint(right);
            let below = geometry.origin_y + i32::try_from(geometry.rows.len()).ok()?;
            let cover = [
                cover.left,
                cover.top,
                cover.right - cover.left,
                cover.bottom - cover.top,
            ];
            row.owned
                .iter()
                .flatten()
                .flat_map(|[left, right]| [[left - 1, y], [*right, y]])
                .chain([[middle, geometry.origin_y - 1], [middle, below]])
                .find(|point| {
                    !geometry.contains_owned(*point)
                        && inside(*point, cover)
                        && !inside(*point, panel)
                })
        }
        "external" => [
            [cover.right - 1, y],
            [cover.left, y],
            [cover.left, cover.top],
            [cover.right - 1, cover.top],
        ]
        .into_iter()
        .find(|point| !inside(*point, commit.proxy)),
        "retract" => {
            let frequency = data.startup.as_ref()?.frequency;
            let mut retracting = false;
            let mut point = None;
            for frame in data.frames.iter().rev().filter(|frame| {
                frame.kind == Kind::Dock
                    && frame.paint_qpc <= commit.qpc
                    && commit.qpc - frame.paint_qpc <= frequency / 20
            }) {
                let old = data.geometries.get(&frame.geometry)?;
                let old_row = old.rows.get(usize::try_from(y - old.origin_y).ok()?)?;
                let old_right = old_row.drawn[0]?[1];
                if !retracting && old_right != right {
                    if old_right < right {
                        return None;
                    }
                    retracting = true;
                }
                let target = [old_right - 1, y];
                if retracting
                    && !geometry.contains_owned(target)
                    && inside(
                        target,
                        [
                            cover.left,
                            cover.top,
                            cover.right - cover.left,
                            cover.bottom - cover.top,
                        ],
                    )
                {
                    point = Some(target);
                    break;
                }
            }
            point
        }
        _ => None,
    };
    point.filter(|point| !inside(*point, panel))
}

#[expect(
    clippy::too_many_lines,
    reason = "keeps every raw receipt beside its native/display disposition and aggregate gate"
)]
pub(super) fn summarize(
    observer: &Path,
    pid: u32,
    presents: &mut PresentRows,
    dock_chain: u64,
    output: &Path,
    mode: Mode,
) -> Result<()> {
    let data = observation::read(observer, pid)?;
    let frequency = data.startup.context("observer startup missing")?.frequency;
    let mut report = File::create_new(output.join("receipt-joins.txt"))?;
    let (displayed, uncertain) =
        joined_frames(&data, presents, dock_chain, frequency, &mut report)?;
    let text = fs::read_to_string(output.join("receipt-trials.csv"))?;
    let motion = if mode == Mode::Motion {
        motion_frames(
            &data,
            &displayed,
            &uncertain,
            presents,
            dock_chain,
            frequency,
            &mut report,
        )
    } else {
        Ok(())
    };
    ensure!(text.ends_with('\n'), "truncated receipt trials");
    let mut lines = text.lines();
    ensure!(lines.next() == Some(HEADER), "invalid receipt trial schema");
    let mut native_measured = BTreeMap::<String, usize>::new();
    let mut displayed_body_counts = BTreeMap::<String, usize>::new();
    let mut display_measured = BTreeMap::<String, usize>::new();
    let mut failures = Vec::new();
    let mut attempted = 0;
    let mut unmeasured = 0;
    for line in lines {
        let fields = line.split(',').collect::<Vec<_>>();
        ensure!(fields.len() == 15, "invalid receipt trial column count");
        ensure!(
            mode.categories().contains(&fields[1]),
            "unexpected receipt category"
        );
        if fields[14] == "no_candidate" {
            writeln!(report, "{line} unmeasured=no_candidate")?;
            continue;
        }
        attempted += 1;
        if fields[14] != "ok" {
            failures.push(format!("trial{} failed", fields[0]));
            writeln!(report, "{line} unmeasured=failed_batch")?;
            continue;
        }
        let point = [fields[2].parse()?, fields[3].parse()?];
        let begin: i64 = fields[7].parse()?;
        let end: i64 = fields[8].parse()?;
        let receipt: i64 = fields[11].parse()?;
        let recipient_count: usize = fields[13].parse()?;
        ensure!(
            begin > 0
                && end >= begin
                && receipt >= begin
                && recipient_count > 0
                && fields[6] == "1"
                && fields[9] == "2",
            "invalid accepted input endpoints"
        );
        let Some(mask) = stable_mask(&data, begin, receipt)? else {
            unmeasured += 1;
            writeln!(report, "{line} unmeasured=mask_transition")?;
            continue;
        };
        let geometry = data
            .geometries
            .get(&mask.geometry)
            .context("mask geometry missing")?;
        let proxy = fields[10] == "proxy";
        ensure!(proxy || fields[10] == "edit", "unknown actual recipient");
        if proxy {
            let actual = data
                .clicks
                .get(recipient_count - 1)
                .context("proxy recipient count missing from observer")?;
            ensure!(
                actual.qpc == receipt
                    && actual.point == point
                    && actual.epoch == fields[12].parse::<u64>()?
                    && actual.epoch == mask.epoch,
                "proxy receipt disagrees with stable mask or raw trial"
            );
        }
        if proxy != geometry.contains_owned(point) {
            failures.push(format!("trial{} native ownership mismatch", fields[0]));
        }
        *native_measured.entry(fields[1].to_owned()).or_default() += 1;
        let visible_body = displayed_body(&data, &displayed, &uncertain, point, begin, receipt);
        if visible_body.is_some() {
            *display_measured.entry(fields[1].to_owned()).or_default() += 1;
        }
        if let Some(true) = visible_body {
            *displayed_body_counts
                .entry(fields[1].to_owned())
                .or_default() += 1;
            if !proxy {
                failures.push(format!("trial{} DISPLAYED BODY LEAK", fields[0]));
            }
        } else if visible_body.is_none() {
            unmeasured += 1;
        }
        writeln!(
            report,
            "{line} native_epoch={} displayed_body={visible_body:?}",
            mask.epoch
        )?;
    }
    writeln!(
        report,
        "attempted={attempted} unmeasured={unmeasured} native_measured={native_measured:?} display_measured={display_measured:?} displayed_body={displayed_body_counts:?} failures={failures:?}"
    )?;
    ensure!(attempted > 0, "no receipt attempts");
    ensure!(
        presents
            .rows()
            .iter()
            .any(|row| row.swapchain == dock_chain),
        "main trace missed the dock seed: no present on the dock swapchain"
    );
    for category in mode.categories() {
        ensure!(
            native_measured.get(*category).copied().unwrap_or(0) > 0
                && display_measured.get(*category).copied().unwrap_or(0) > 0,
            "{category} native/display receipt coverage unmeasured"
        );
    }
    let body_categories: &[&str] = match mode {
        Mode::Rest => &["center", "edge"],
        Mode::Motion => &["body", "retract"],
    };
    for category in body_categories {
        ensure!(
            displayed_body_counts.get(*category).copied().unwrap_or(0) > 0,
            "{category} displayed-body receipts unmeasured"
        );
    }
    if let Err(error) = motion {
        failures.push(format!("motion frame gate: {error:#}"));
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn stable_mask(data: &Observation, begin: i64, receipt: i64) -> Result<Option<&Commit>> {
    let mask = data
        .commits
        .iter()
        .rev()
        .find(|commit| commit.qpc <= begin)
        .context("no committed mask before click")?;
    let unstable = data.commits.iter().any(|commit| {
        commit.epoch != mask.epoch && commit.begin_qpc <= receipt && commit.qpc >= begin
    });
    Ok((!unstable).then_some(mask))
}

fn joined_frames<'a>(
    data: &'a Observation,
    presents: &mut PresentRows,
    chain: u64,
    frequency: i64,
    report: &mut File,
) -> Result<(Vec<Displayed>, Vec<&'a Frame>)> {
    let mut displayed = Vec::new();
    let mut uncertain = Vec::new();
    for frame in data
        .frames
        .iter()
        .filter(|frame| frame.kind == Kind::Dock && frame.geometry != 0)
    {
        let joined = presents.join(
            chain,
            frame.paint_qpc,
            frame.completion_qpc.unwrap_or(0),
            frequency,
        );
        let provenance = presents
            .candidates(frequency)
            .filter(|candidate| {
                candidate.present.swapchain == chain
                    && (frame.paint_qpc..=frame.completion_qpc.unwrap_or(0))
                        .contains(&candidate.present.qpc)
            })
            .map(|candidate| candidate.source)
            .collect::<Vec<_>>();
        writeln!(
            report,
            "frame={} geometry={} join={joined:?} provenance={provenance:?}",
            frame.id, frame.geometry
        )?;
        match joined {
            Join::Displayed {
                present_qpc,
                displayed_qpc,
            } => displayed.push(Displayed {
                at: displayed_qpc,
                painted: frame.paint_qpc,
                geometry: frame.geometry,
                present: present_qpc,
                frame: frame.id,
            }),
            Join::Dropped => {}
            _ => uncertain.push(frame),
        }
    }
    displayed.sort_unstable_by_key(|frame| frame.at);
    Ok((displayed, uncertain))
}

fn same_drawn(left: &Geometry, right: &Geometry) -> bool {
    let pixels = |geometry: &Geometry| {
        geometry
            .rows
            .iter()
            .enumerate()
            .flat_map(|(index, row)| {
                row.drawn.iter().flatten().map(move |&span| {
                    (
                        i64::from(geometry.origin_y)
                            + i64::try_from(index).expect("observer row count is bounded512"),
                        span,
                    )
                })
            })
            .collect::<Vec<_>>()
    };
    pixels(left) == pixels(right)
}

fn demand_spans(data: &Observation) -> Result<Vec<(i64, i64)>> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut previous = None;
    for commit in &data.commits {
        let geometry = data
            .geometries
            .get(&commit.geometry)
            .context("motion geometry missing")?;
        let changed = previous.is_some_and(|before| !same_drawn(before, geometry));
        if changed {
            start.get_or_insert(commit.qpc);
        } else if let Some(begin) = start.take() {
            spans.push((begin, commit.qpc));
        }
        previous = Some(geometry);
    }
    if let (Some(begin), Some(last)) = (start, data.commits.last()) {
        spans.push((begin, last.qpc));
    }
    Ok(spans)
}

fn motion_frames(
    data: &Observation,
    displayed: &[Displayed],
    uncertain: &[&Frame],
    presents: &PresentRows,
    chain: u64,
    frequency: i64,
    report: &mut File,
) -> Result<()> {
    let spans = demand_spans(data)?;
    writeln!(
        report,
        "animation_demand_spans={spans:?}; derived from exact current-row changes at every committed fixture tick; unchanged-pixel gaps are not missed frames"
    )?;
    let demanded = |qpc| {
        spans
            .iter()
            .any(|(begin, end)| *begin <= qpc && qpc <= *end)
    };
    let candidates = presents.candidates(frequency).collect::<Vec<_>>();
    let first = candidates
        .iter()
        .map(|candidate| &candidate.present)
        .filter(|row| row.swapchain == chain)
        .map(|row| row.qpc)
        .min()
        .context("motion trace has no dock rows")?;
    let last = candidates
        .iter()
        .map(|candidate| &candidate.present)
        .filter(|row| row.swapchain == chain)
        .map(|row| row.qpc)
        .max()
        .context("motion trace has no dock rows")?;
    writeln!(
        report,
        "motion_capture_observed_qpc={first}..={last}; pre-trace warmup and post-trace paints are outside this measured window"
    )?;
    let missing = uncertain
        .iter()
        .filter(|frame| {
            frame.paint_qpc >= first && frame.paint_qpc <= last && demanded(frame.paint_qpc)
        })
        .count();
    let dropped = presents
        .rows()
        .iter()
        .filter(|row| row.swapchain == chain && row.dropped)
        .count();
    let dropped_on_demand = presents
        .rows()
        .iter()
        .filter(|row| row.swapchain == chain && row.dropped && demanded(row.qpc))
        .count();
    let mut intervals = Vec::new();
    for pair in displayed.windows(2) {
        if pair[0].painted >= first
            && pair[1].painted <= last
            && spans
                .iter()
                .any(|(begin, end)| *begin <= pair[0].painted && pair[1].painted <= *end)
        {
            let delta = pair[1]
                .at
                .checked_sub(pair[0].at)
                .context("display clock went backwards")?;
            ensure!(delta > 0, "ambiguous simultaneous displayed dock frames");
            intervals.push(delta);
            writeln!(
                report,
                "motion_display_gap frames={},{} ticks={delta}",
                pair[0].frame, pair[1].frame
            )?;
        }
    }
    intervals.sort_unstable();
    ensure!(
        !intervals.is_empty(),
        "moving-frame display gaps unmeasured"
    );
    let p99 = intervals[(intervals.len() * 99).div_ceil(100) - 1];
    let max = intervals[intervals.len() - 1];
    writeln!(
        report,
        "motion_gap_n={} p99_ticks={p99} max_ticks={max} frequency={frequency} unknown_demanded_frames={missing} undisplayed_dock_rows={dropped} undisplayed_demanded_rows={dropped_on_demand}; all dropped rows retained",
        intervals.len()
    )?;
    ensure!(missing == 0, "moving-frame joins incomplete: {missing}");
    ensure!(
        dropped_on_demand == 0,
        "moving-frame demand contains undisplayed presents: {dropped_on_demand}"
    );
    ensure!(
        i128::from(p99) * 10_000 <= i128::from(frequency) * 77,
        "moving-frame displayed-gap p99 exceeds7.7ms"
    );
    Ok(())
}

fn displayed_body(
    data: &Observation,
    displayed: &[Displayed],
    uncertain: &[&Frame],
    point: [i32; 2],
    begin: i64,
    receipt: i64,
) -> Option<bool> {
    let shown = displayed.iter().rfind(|frame| frame.at <= begin)?;
    if displayed
        .iter()
        .filter(|frame| frame.at == shown.at)
        .count()
        != 1
        || data.frames.iter().any(|frame| {
            frame.kind == Kind::Dock
                && frame.id != shown.frame
                && frame.paint_qpc <= shown.present
                && frame.completion_qpc.is_some_and(|end| shown.present <= end)
        })
        || uncertain
            .iter()
            .any(|frame| frame.paint_qpc > shown.painted && frame.paint_qpc <= receipt)
        || displayed.iter().any(|frame| {
            frame.at > begin && frame.at <= receipt && frame.geometry != shown.geometry
        })
    {
        return None;
    }
    data.geometries
        .get(&shown.geometry)
        .map(|geometry| geometry.contains_drawn(point))
}

#[cfg(test)]
mod tests;
