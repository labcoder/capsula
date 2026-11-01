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
    Motion,
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
        let categories: &[&str] = if mode == Mode::Rest {
            &["center", "edge", "outside", "external"]
        } else {
            &["body", "previous", "outside", "external", "retract"]
        };
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
    let commit = data.commits.last()?;
    let geometry = data.geometries.get(&commit.geometry)?;
    let y = dock[1] + dock[3] / 2;
    let row = geometry
        .rows
        .get(usize::try_from(y - geometry.origin_y).ok()?)?;
    let [left, right] = row.drawn?;
    match category {
        "center" => Some([left.midpoint(right), y]),
        "edge" => Some([left, y]),
        "body" => (right > dock[0] + dock[2]).then_some([right - 1, y]),
        "previous" => row.owned.iter().flatten().find_map(|span| {
            [span[0], span[1] - 1]
                .into_iter()
                .find(|x| !geometry.contains_drawn([*x, y]))
                .map(|x| [x, y])
        }),
        "outside" => (commit.proxy[0]..commit.proxy[0] + commit.proxy[2])
            .map(|x| [x, y])
            .find(|point| !geometry.contains_owned(*point)),
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
            for frame in data.frames.iter().rev().filter(|frame| {
                frame.kind == Kind::Dock
                    && frame.paint_qpc <= commit.qpc
                    && commit.qpc - frame.paint_qpc <= frequency / 20
            }) {
                let old = data.geometries.get(&frame.geometry)?;
                let old_row = old.rows.get(usize::try_from(y - old.origin_y).ok()?)?;
                let old_right = old_row.drawn?[1];
                if !retracting && old_right != right {
                    if old_right < right {
                        return None;
                    }
                    retracting = true;
                }
                let point = [old_right - 1, y];
                if retracting
                    && !geometry.contains_owned(point)
                    && inside(
                        point,
                        [
                            cover.left,
                            cover.top,
                            cover.right - cover.left,
                            cover.bottom - cover.top,
                        ],
                    )
                {
                    return Some(point);
                }
            }
            None
        }
        _ => None,
    }
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
) -> Result<()> {
    let data = observation::read(observer, pid)?;
    let frequency = data.startup.context("observer startup missing")?.frequency;
    let mut report = File::create_new(output.join("receipt-joins.txt"))?;
    let (displayed, uncertain) =
        joined_frames(&data, presents, dock_chain, frequency, &mut report)?;
    let text = fs::read_to_string(output.join("receipt-trials.csv"))?;
    let motion = if text.contains(",retract,") {
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
    let mut failures = Vec::new();
    let mut attempted = 0;
    let mut unmeasured = 0;
    for line in lines {
        let fields = line.split(',').collect::<Vec<_>>();
        ensure!(fields.len() == 15, "invalid receipt trial column count");
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
        "attempted={attempted} unmeasured={unmeasured} native_measured={native_measured:?} displayed_body={displayed_body_counts:?} failures={failures:?}"
    )?;
    ensure!(attempted > 0, "no receipt attempts");
    ensure!(
        native_measured.values().sum::<usize>() > 0,
        "no stable native ownership joins"
    );
    if text.contains(",retract,") {
        ensure!(
            displayed_body_counts.get("retract").copied().unwrap_or(0) > 0,
            "retracting-edge displayed-body receipts unmeasured"
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
        writeln!(
            report,
            "frame={} geometry={} join={joined:?}",
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
            .filter_map(|(index, row)| {
                row.drawn.map(|span| {
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
    let first = presents
        .rows()
        .iter()
        .filter(|row| row.swapchain == chain)
        .map(|row| row.qpc)
        .min()
        .context("motion trace has no dock rows")?;
    let last = presents
        .rows()
        .iter()
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
        "motion_gap_n={} p99_ticks={p99} max_ticks={max} frequency={frequency} unknown_demanded_frames={missing} undisplayed_dock_rows={dropped}; all dropped rows retained",
        intervals.len()
    )?;
    ensure!(missing == 0, "moving-frame joins incomplete: {missing}");
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
mod tests {
    use super::observation::{Commit, Geometry, Row, Startup};
    use super::*;

    fn sample_data() -> Observation {
        let mut data = Observation::default();
        data.startup = Some(Startup {
            panel_hwnd: 2,
            frequency: 1000,
            work_area: [0, 0, 1000, 1000],
            dock: [0, 0, 100, 40],
            overlay: [0, 0, 200, 40],
            panel: [0, 44, 200, 100],
        });
        for (id, right) in [(1, 120), (2, 140), (3, 130), (4, 110)] {
            data.geometries.insert(
                id,
                Geometry {
                    origin_y: 20,
                    rows: vec![Row {
                        drawn: Some([0, right]),
                        owned: [Some([0, right.max(130)]), None],
                    }],
                },
            );
        }
        data.commits.push(Commit {
            begin_qpc: 99,
            qpc: 100,
            epoch: 1,
            geometry: 1,
            proxy: [0, 0, 200, 40],
        });
        data
    }

    fn frame(id: u64, geometry: u64, qpc: i64) -> Frame {
        Frame {
            id,
            geometry,
            paint_qpc: qpc,
            completion_qpc: Some(qpc + 1),
            kind: Kind::Dock,
            hover: 0,
        }
    }

    #[test]
    fn retract_targets_require_recent_contraction_not_an_older_cycle_peak() {
        let mut data = sample_data();
        data.frames = vec![frame(1, 2, 90), frame(2, 3, 95), frame(3, 1, 98)];
        let cover = RECT {
            left: 0,
            top: 0,
            right: 208,
            bottom: 40,
        };
        assert_eq!(
            candidate(&data, "retract", [0, 0, 100, 40], cover),
            Some([139, 20])
        );
        data.frames[1] = frame(2, 4, 95);
        assert_eq!(candidate(&data, "retract", [0, 0, 100, 40], cover), None);
        assert_eq!(
            candidate(&data, "edge", [0, 0, 100, 40], cover),
            Some([0, 20])
        );
        assert_eq!(
            candidate(&data, "previous", [0, 0, 100, 40], cover),
            Some([129, 20])
        );
        assert_eq!(
            candidate(&data, "outside", [0, 0, 100, 40], cover),
            Some([130, 20])
        );
    }

    #[test]
    fn newer_unknown_or_overlapping_frames_never_reuse_an_older_display_claim() {
        let mut data = sample_data();
        data.frames = vec![frame(1, 1, 90)];
        let displayed = [Displayed {
            at: 95,
            painted: 90,
            geometry: 1,
            present: 91,
            frame: 1,
        }];
        assert_eq!(
            displayed_body(&data, &displayed, &[], [119, 20], 100, 102),
            Some(true)
        );
        assert_eq!(
            displayed_body(&data, &displayed, &[], [120, 20], 100, 102),
            Some(false)
        );
        let unknown = frame(2, 2, 98);
        assert_eq!(
            displayed_body(&data, &displayed, &[&unknown], [119, 20], 100, 102),
            None
        );
        data.frames.push(Frame {
            completion_qpc: Some(93),
            ..frame(3, 2, 89)
        });
        assert_eq!(
            displayed_body(&data, &displayed, &[], [119, 20], 100, 102),
            None
        );
    }

    #[test]
    fn a_mask_upload_in_flight_at_the_receipt_is_not_a_stable_mask() {
        let mut data = sample_data();
        assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 1);
        data.commits.push(Commit {
            begin_qpc: 107,
            qpc: 120,
            epoch: 2,
            geometry: 2,
            proxy: [0, 0, 200, 40],
        });
        assert!(stable_mask(&data, 105, 108).unwrap().is_none());
        data.commits[1].begin_qpc = 109;
        assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 1);
        data.commits[1].begin_qpc = 101;
        data.commits[1].qpc = 103;
        assert_eq!(stable_mask(&data, 105, 108).unwrap().unwrap().epoch, 2);
    }

    #[test]
    fn drawing_demand_starts_only_on_changed_current_pixels_and_keeps_gaps() {
        let mut data = sample_data();
        for (qpc, geometry) in [(101, 1), (102, 2), (103, 3), (104, 3), (105, 4)] {
            data.commits.push(Commit {
                begin_qpc: qpc,
                qpc,
                epoch: geometry,
                geometry,
                proxy: [0, 0, 200, 40],
            });
        }
        assert_eq!(demand_spans(&data).unwrap(), vec![(102, 104), (105, 105)]);
        let padded = Geometry {
            origin_y: 19,
            rows: vec![
                Row {
                    drawn: None,
                    owned: [Some([0, 140]), None],
                },
                Row {
                    drawn: Some([0, 120]),
                    owned: [Some([0, 140]), None],
                },
            ],
        };
        assert!(
            same_drawn(&data.geometries[&1], &padded),
            "ownership padding isn't drawing demand"
        );
    }

    #[test]
    fn cover_clamps_inside_the_work_area_without_losing_the_overlay() {
        assert_eq!(
            cover_rect([0, 100, 336, 272], [0, 0, 2560, 1400]).unwrap(),
            RECT {
                left: 0,
                top: 92,
                right: 344,
                bottom: 380
            }
        );
        assert!(cover_rect([0, 0, 1100, 100], [0, 0, 2560, 1440]).is_err());
        assert!(inside([3, 4], [3, 4, 2, 2]));
        assert!(!inside([5, 4], [3, 4, 2, 2]));
    }
}
