//! `cargo xtask slice2-gate`: the slice 2 gate. It runs the real dev brain with fixture scripts and
//! drives its real pipes from a scripted client, with no mouse or keyboard input.
//!
//! The brain only serves the `kumochi.exe` beside it, so the gate copies the brain and itself, as
//! `kumochi.exe`, into a fresh folder under `target/slice2-gate` and runs the client from that copy.
//! Each item gets its own `%LOCALAPPDATA%`, so no item sees another's store, and the developer's own
//! dev store is never touched.
#![expect(
    clippy::print_stdout,
    reason = "the gate's verdicts and numbers are its output"
)]

mod brain;
pub(crate) mod client;
mod delete;
mod recover;
mod stop;
mod store;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, SHELL_EXE};
use kumochi_protocol::message::{ControlToShell, DataToBrain, DataToShell, Phase, RunId, ThreadId};

use crate::platform;
use client::{Client, Frame};

/// One gate item, run in its own folder.
type Item = fn(&Path) -> Result<Row>;

/// The argument the copied gate runs with.
const INNER: &str = "--inner";
/// How long the whole gate may run before it and its brains are killed.
const GATE_LIMIT: Duration = Duration::from_mins(20);
/// How long one expected frame may take before the item fails.
const PATIENCE: Duration = Duration::from_secs(10);

/// One gate item's verdict and its numbers.
#[derive(Debug)]
struct Row {
    item: &'static str,
    verdict: Verdict,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    NotRun,
}

impl Row {
    fn checked(item: &'static str, failures: &[String], detail: String) -> Self {
        let verdict = if failures.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        let mut detail = detail;
        for failure in failures {
            let _infallible = write!(detail, "; FAIL: {failure}");
        }
        Self {
            item,
            verdict,
            detail,
        }
    }
}

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    match args.next() {
        Some(inner) if inner == INNER => {
            let folder = args.next().context("the inner gate needs its folder")?;
            gate(Path::new(&folder), args.next().as_deref())
        }
        only => {
            ensure!(
                args.next().is_none(),
                "slice2-gate takes at most one item filter"
            );
            prepare(only.as_deref())
        }
    }
}

/// Builds the dev brain, copies it and this program into a fresh folder, and runs the gate there:
/// every item, or only those whose names hold the filter.
fn prepare(only: Option<&str>) -> Result<()> {
    ensure!(
        cfg!(debug_assertions),
        "run the gate from a debug xtask (`cargo xtask slice2-gate`): a release one would copy a \
         release brain, which uses the installed product's pipe and credential names"
    );
    ensure!(
        platform::processes()?.is_empty(),
        "close every {SHELL_EXE} and {BRAIN_EXE} first: a dev brain holds the pipes the gate uses"
    );
    let built = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "--quiet", "--package", "kumochi-brain"])
        .status()
        .context("building the brain")?;
    ensure!(built.success(), "the brain didn't build");
    let me = std::env::current_exe()?;
    let target = me.parent().context("xtask has a folder")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let folder = target
        .parent()
        .context("the profile folder has a parent")?
        .join("slice2-gate")
        .join(stamp.to_string());
    let bin = folder.join("bin");
    fs::create_dir_all(&bin)?;
    fs::copy(target.join(BRAIN_EXE), bin.join(BRAIN_EXE)).context("copying the brain")?;
    fs::copy(&me, bin.join(SHELL_EXE)).context("copying the client")?;
    println!("slice 2 gate in {}", folder.display());
    let mut gate = Command::new(bin.join(SHELL_EXE))
        .args(["slice2-gate", INNER])
        .arg(&folder)
        .args(only)
        .spawn()
        .context("running the gate")?;
    let end = Instant::now() + GATE_LIMIT;
    while gate.try_wait()?.is_none() {
        if Instant::now() >= end {
            let brains: Vec<_> = platform::processes()?
                .into_iter()
                .filter(|entry| entry.parent == gate.id())
                .collect();
            gate.kill()?;
            gate.wait()?;
            for brain in brains {
                let _gone = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &brain.pid.to_string()])
                    .status();
            }
            bail!("the gate didn't finish in {GATE_LIMIT:?}; it and its brains were killed");
        }
        thread::sleep(Duration::from_millis(200));
    }
    ensure!(gate.wait()?.success(), "the gate failed");
    Ok(())
}

fn gate(folder: &Path, only: Option<&str>) -> Result<()> {
    let items: [(&'static str, Item); 7] = [
        ("Esc against a non-reading shell", stop::not_reading),
        ("Esc against a saturated data writer", stop::saturated),
        ("Replacement run after Esc", stop::replacement),
        ("Late closed step after Stop refused", stop::late_step),
        ("Kill mid-run, then recover", recover::kill),
        (
            "Reconnect with a multi-chunk drawing while Stop arrives",
            recover::reconnect,
        ),
        ("§6.4 deletion on fixture data", delete::run),
    ];
    let mut rows = Vec::new();
    for (name, item) in items {
        if only.is_some_and(|only| !name.contains(only)) {
            continue;
        }
        println!("running: {name}");
        match item(folder) {
            Ok(row) => rows.push(row),
            Err(error) => rows.push(Row {
                item: name,
                verdict: Verdict::Fail,
                detail: format!("{error:#}"),
            }),
        }
    }
    rows.extend(only.is_none().then(|| Row {
        item: "Esc on the real key in the real shell",
        verdict: Verdict::NotRun,
        detail: "needs an input session on the desk (OSC-132)".to_owned(),
    }));
    let report = table(&rows);
    println!("{report}");
    fs::write(folder.join("report.md"), &report)?;
    ensure!(
        rows.iter().all(|row| row.verdict != Verdict::Fail),
        "a gate item failed"
    );
    Ok(())
}

fn table(rows: &[Row]) -> String {
    let mut out = "| Gate item | Result | Numbers |\n|---|---|---|\n".to_owned();
    for row in rows {
        let verdict = match row.verdict {
            Verdict::Pass => "pass",
            Verdict::Fail => "**FAIL**",
            Verdict::NotRun => "not run",
        };
        let _infallible = writeln!(out, "| {} | {verdict} | {} |", row.item, row.detail);
    }
    out
}

/// The nearest-rank percentile of `samples`, in milliseconds.
fn percentile(samples: &[Duration], percent: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted
        .get(rank - 1)
        .map_or(f64::NAN, |sample| sample.as_secs_f64() * 1000.0)
}

/// Sends a turn and waits for its run to be named.
fn turn(client: &mut Client, thread: Option<ThreadId>, text: &str) -> Result<(RunId, ThreadId)> {
    client.data(&DataToBrain::TextSubmit {
        turn: 1,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread,
        text: text.to_owned(),
    })?;
    client.until(PATIENCE, |seen| match seen.frame {
        Frame::Control(ControlToShell::RunState {
            run,
            thread,
            phase: Phase::Thinking,
            ..
        }) => Some((run, thread)),
        _ => None,
    })
}

/// Waits for `run` to reach a terminal phase, and returns it with when it arrived.
fn ended(client: &mut Client, run: RunId) -> Result<(Instant, Phase)> {
    client.taken_or_until(PATIENCE, |seen| match seen.frame {
        Frame::Control(ControlToShell::RunState {
            run: named, phase, ..
        }) if named == run && matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed) => {
            Some((seen.at, phase))
        }
        _ => None,
    })
}

/// Waits for `run` 's last data frame.
fn run_end(client: &mut Client, run: RunId) -> Result<()> {
    client.taken_or_until(PATIENCE, |seen| match seen.frame {
        Frame::Data(DataToShell::RunEnd { run: ended }, _) if ended == run => Some(()),
        _ => None,
    })
}

/// Waits for `run` 's first text.
fn first_text(client: &mut Client, run: RunId) -> Result<()> {
    client.taken_or_until(PATIENCE, |seen| match &seen.frame {
        Frame::Data(DataToShell::AssistantText { run: named, .. }, _) if *named == run => Some(()),
        _ => None,
    })
}

/// A random lowercase word no fixture or store holds by chance.
fn marker() -> String {
    use std::hash::{BuildHasher as _, RandomState};
    format!(
        "capsgate{:016x}",
        RandomState::new().hash_one(SystemTime::now())
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::percentile;

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let samples: Vec<_> = (1..=20).rev().map(Duration::from_millis).collect();
        assert!((percentile(&samples, 50) - 10.0).abs() < 1e-9, "p50");
        assert!((percentile(&samples, 95) - 19.0).abs() < 1e-9, "p95");
        assert!((percentile(&samples, 100) - 20.0).abs() < 1e-9, "max");
        assert!(percentile(&[], 95).is_nan(), "no samples");
    }
}
