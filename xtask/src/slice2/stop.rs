//! Esc against a shell that isn't reading and against a saturated data writer, a replacement
//! turn right after Esc, and a step the engine finishes after Stop.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DATA_WINDOW, DataToBrain, DataToShell, Phase, RunId,
};

use super::brain::Case;
use super::client::{Client, Frame};
use super::{GONE, PATIENCE, Row, ended, first_text, marker, percentile, run_end, store, turn};

/// Stops measured for each kind of pressure.
const REPEATS: usize = 20;
/// The p95 a Stop must be answered within [guess].
const STOP_P95_MS: f64 = 50.0;
/// The saturated fixture's fills: 17 of 61,000 bytes stay inside the 1 MiB text budget, and
/// once 8 have gone out on credit the other 9 are more than the brain's data queue holds.
const FILLS: u64 = 17;
const FILL: usize = 61_000;
/// The brain's data-queue cap, `DATA_CAP` in its runtime: at or above it, engine events wait.
const DATA_CAP: u64 = 512 * 1024;
/// Replacement turns sent right behind a Stop.
const REPLACEMENTS: usize = 10;
/// The most a Stop and the replacement turn may be apart.
const REPLACE_GAP: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pressure {
    /// The client stops reading the data pipe.
    NotReading,
    /// The client reads but grants no credit, while the fixture streams its whole text budget.
    Saturated,
}

pub(super) fn not_reading(folder: &Path) -> Result<Row> {
    esc(folder, Pressure::NotReading)
}

pub(super) fn saturated(folder: &Path) -> Result<Row> {
    esc(folder, Pressure::Saturated)
}

fn esc(folder: &Path, pressure: Pressure) -> Result<Row> {
    let (name, item, script) = match pressure {
        Pressure::NotReading => (
            "esc-not-reading",
            "Esc against a non-reading shell",
            format!("0 text start\n{}", "10 fill 4096\n".repeat(200)),
        ),
        // The last line keeps the run going past the Stop.
        Pressure::Saturated => (
            "esc-saturated",
            "Esc against a saturated data writer",
            format!(
                "0 text start\n{}5000 text end\n",
                format!("0 fill {FILL}\n").repeat(usize::try_from(FILLS)?)
            ),
        ),
    };
    let case = Case::new(folder, name, &script)?;
    let brain = case.start()?;
    let mut client = Client::connect(brain.pid())?;
    let mut latencies = Vec::new();
    let mut runs = Vec::new();
    let mut taken = Vec::new();
    let mut failures = Vec::new();
    let mut thread = None;
    for _ in 0..REPEATS {
        if pressure == Pressure::Saturated {
            client.withhold_credit();
        }
        let (run, named) = turn(&mut client, thread, "gate stop")?;
        thread = Some(named);
        first_text(&mut client, run)?;
        if pressure == Pressure::NotReading {
            client.pause_data();
        }
        client.drain(Duration::from_millis(300));
        taken.push(fills(&client, run));
        let sent = client.control(&ControlToBrain::Stop {
            run,
            revealed: Vec::new(),
        })?;
        let (at, phase) = ended(&mut client, run)?;
        latencies.push(at.saturating_duration_since(sent));
        if phase != Phase::Interrupted {
            failures.push(format!("run {} ended {phase:?}", run.0));
        }
        client.resume_data();
        client.grant()?;
        run_end(&mut client, run)?;
        runs.push(run);
    }
    client.quit()?;
    brain.exited()?;
    let conn = store::open(&case.store())?;
    for run in &runs {
        let outcome = store::outcome(&conn, run.0)?;
        if outcome != Some(("interrupted".to_owned(), Some("stop".to_owned()))) {
            failures.push(format!("run {} stored as {outcome:?}", run.0));
        }
    }
    let (p50, p95) = (percentile(&latencies, 50), percentile(&latencies, 95));
    let max = percentile(&latencies, 100);
    if p95 >= STOP_P95_MS {
        failures.push(format!("p95 {p95:.1} ms is over {STOP_P95_MS} ms"));
    }
    let mut detail = format!(
        "n={REPEATS}; Stop written → RunState Interrupted read: p50 {p50:.1} ms, p95 {p95:.1} ms, \
         max {max:.1} ms; every run stored interrupted(stop), none view_stalled"
    );
    if pressure == Pressure::Saturated {
        let window = u64::from(DATA_WINDOW);
        for (count, frame, bytes) in &taken {
            let queued = FILLS.saturating_sub(*count) * frame;
            if bytes + frame <= window || *bytes > window {
                failures.push(format!("{bytes} B taken leaves room in the credit window"));
            }
            if queued <= DATA_CAP {
                failures.push(format!(
                    "{queued} B unsent isn't over the {DATA_CAP} B queue cap"
                ));
            }
        }
        let (count, frame, bytes) = taken.iter().min().copied().unwrap_or_default();
        let unsent = FILLS.saturating_sub(count);
        let _infallible = write!(
            detail,
            "; before every Stop the client had taken {count} of {FILLS} fill frames of {frame} B \
             ({bytes} B, no room for another in the {window} B credit window), so the {unsent} \
             unsent are {} B, over the {DATA_CAP} B queue cap",
            unsent * frame
        );
    }
    Ok(Row::checked(item, &failures, detail))
}

/// The fill frames the client has taken for `run`, the largest one's size, and every byte of its
/// text taken, headers included.
fn fills(client: &Client, run: RunId) -> (u64, u64, u64) {
    let mut taken = (0, 0, 0);
    for seen in &client.log {
        if let Frame::Data(DataToShell::AssistantText { run: named, delta }, bytes) = &seen.frame
            && *named == run
        {
            let bytes = u64::from(*bytes);
            if delta.len() == FILL {
                taken.0 += 1;
                taken.1 = taken.1.max(bytes);
            }
            taken.2 += bytes;
        }
    }
    taken
}

pub(super) fn replacement(folder: &Path) -> Result<Row> {
    let script = format!("0 text a\n{}", "20 text b\n".repeat(400));
    let case = Case::new(folder, "replacement", &script)?;
    let brain = case.start()?;
    let mut client = Client::connect(brain.pid())?;
    let (mut run, thread) = turn(&mut client, None, "gate replace")?;
    first_text(&mut client, run)?;
    let mut gaps = Vec::new();
    let mut pairs = Vec::new();
    for _ in 0..REPLACEMENTS {
        let stopped = client.control(&ControlToBrain::Stop {
            run,
            revealed: Vec::new(),
        })?;
        let submitted = client.data(&DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(thread),
            text: "gate replace".into(),
        })?;
        gaps.push(submitted.saturating_duration_since(stopped));
        let old = run;
        run = client.until(PATIENCE, |seen| match seen.frame {
            Frame::Control(ControlToShell::RunState {
                run: named,
                phase: Phase::Thinking,
                ..
            }) if named != old => Some(named),
            _ => None,
        })?;
        pairs.push((old, run));
        first_text(&mut client, run)?;
    }
    client.control(&ControlToBrain::Stop {
        run,
        revealed: Vec::new(),
    })?;
    client.drain(Duration::from_secs(1));
    let mut failures = Vec::new();
    let mut late = 0;
    for (old, new) in &pairs {
        late += replaced(&client, *old, *new, &mut failures);
    }
    client.quit()?;
    brain.exited()?;
    let conn = store::open(&case.store())?;
    let mut stopped = 0;
    for (old, _) in &pairs {
        match store::outcome(&conn, old.0)? {
            Some((state, Some(reason))) if state == "interrupted" => {
                stopped += usize::from(reason == "stop");
            }
            other => failures.push(format!("run {} stored as {other:?}", old.0)),
        }
    }
    let gap = percentile(&gaps, 100);
    if gaps.iter().any(|gap| *gap >= REPLACE_GAP) {
        failures.push(format!("a turn went {gap:.3} ms after its Stop"));
    }
    let detail = format!(
        "n={REPLACEMENTS}; Stop → TextSubmit at most {gap:.3} ms apart; every new run id higher, \
         the old run's last state Interrupted and its RunEnd before any of the new run's data; \
         {stopped} stored interrupted(stop), the rest interrupted(replaced); {late} old-run \
         frames already in the pipe were read after Interrupted, all before its RunEnd"
    );
    Ok(Row::checked("Replacement run after Esc", &failures, detail))
}

/// Checks one replacement in the client's record, and counts the old run's data frames read
/// after its `Interrupted`.
fn replaced(client: &Client, old: RunId, new: RunId, failures: &mut Vec<String>) -> usize {
    if new <= old {
        failures.push(format!("run {} replaced by lower run {}", old.0, new.0));
    }
    let states: Vec<_> = client
        .log
        .iter()
        .filter_map(|seen| match seen.frame {
            Frame::Control(ControlToShell::RunState { run, phase, .. }) if run == old => {
                Some((seen.at, phase))
            }
            _ => None,
        })
        .collect();
    let Some(&(interrupted, Phase::Interrupted)) = states.last() else {
        failures.push(format!("run {} didn't end Interrupted: {states:?}", old.0));
        return 0;
    };
    let data: Vec<_> = client
        .log
        .iter()
        .filter_map(|seen| match &seen.frame {
            Frame::Data(message, _) => Some((seen.at, message)),
            _ => None,
        })
        .collect();
    let of = |message: &DataToShell, run: RunId| match message {
        DataToShell::AssistantText { run: named, .. }
        | DataToShell::StepCommit { run: named, .. }
        | DataToShell::RunEnd { run: named } => *named == run,
        _ => false,
    };
    let end = data
        .iter()
        .position(|(_, message)| matches!(message, DataToShell::RunEnd { run } if *run == old));
    let first_new = data.iter().position(|(_, message)| of(message, new));
    match (end, first_new) {
        (Some(end), Some(first)) if end < first => {}
        _ => failures.push(format!(
            "run {}'s RunEnd ({end:?}) isn't before run {}'s first data ({first_new:?})",
            old.0, new.0
        )),
    }
    if end.is_some_and(|end| data[end + 1..].iter().any(|(_, message)| of(message, old))) {
        failures.push(format!("run {} sent data after its RunEnd", old.0));
    }
    data.iter()
        .filter(|(at, message)| of(message, old) && *at > interrupted)
        .count()
}

pub(super) fn late_step(folder: &Path) -> Result<Row> {
    let late = marker(GONE);
    let script = format!("0 text a\n0 step first\n5000 step never\nlate {late}\n");
    let case = Case::new(folder, "late-step", &script)?;
    let brain = case.start()?;
    let mut client = Client::connect(brain.pid())?;
    let (run, _) = turn(&mut client, None, "gate late")?;
    client.until(PATIENCE, |seen| match seen.frame {
        Frame::Data(DataToShell::StepCommit { run: named, .. }, _) if named == run => Some(()),
        _ => None,
    })?;
    client.control(&ControlToBrain::Stop {
        run,
        revealed: vec![0],
    })?;
    let (_, phase) = ended(&mut client, run)?;
    run_end(&mut client, run)?;
    client.drain(Duration::from_secs(1));
    let mut failures = Vec::new();
    if phase != Phase::Interrupted {
        failures.push(format!("the run ended {phase:?}"));
    }
    let commits = client
        .log
        .iter()
        .filter(|seen| matches!(seen.frame, Frame::Data(DataToShell::StepCommit { .. }, _)))
        .count();
    if commits != 1 {
        failures.push(format!("{commits} StepCommits reached the pipe, not 1"));
    }
    client.quit()?;
    brain.exited()?;
    let (files, file_hits) = store::files(&case.folders.local, &late)?;
    let rows = store::rows(&store::open(&case.store())?, &late)?;
    if !rows.hits.is_empty() || rows.fts > 0 || !file_hits.is_empty() {
        failures.push(format!(
            "the late step is stored: tables {:?}, FTS {}, files {file_hits:?}",
            rows.hits, rows.fts
        ));
    }
    let detail = format!(
        "the late step after Stop was refused: 1 StepCommit (the step before Stop), the run \
         ended Interrupted, and its label is in none of {} tables ({} values), FTS or {files} \
         files; the brain's Stale is internal and is seen only through these effects",
        rows.tables, rows.values
    );
    Ok(Row::checked(
        "Late closed step after Stop refused",
        &failures,
        detail,
    ))
}
