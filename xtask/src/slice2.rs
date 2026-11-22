//! `slice2-gate`: the slice 2 gate. It runs the real brain, built with the gate identity, with
//! fixture scripts and drives its real pipes from a scripted client, with no mouse or keyboard input.
//!
//! The brain only serves the `kumochi.exe` beside it, so the gate copies the brain and itself, as
//! `kumochi.exe`, into a fresh folder under `target/slice2-gate` and runs the client from that copy.
//! Each item gets its own `%LOCALAPPDATA%` and `%APPDATA%`, with Claude Code off, and the run
//! deletes what its brains left: see `docs/development/gate-identity.md`.
#![expect(
    clippy::print_stdout,
    reason = "the gate's verdicts and numbers are its output"
)]

mod brain;
pub(crate) mod client;
mod delete;
mod recover;
mod stop;
pub(crate) mod store;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, ProductIdentity, SHELL_EXE};
use kumochi_protocol::message::{ControlToShell, DataToBrain, DataToShell, Phase, RunId, ThreadId};

use crate::hermetic::{self, Profile, Run};
use crate::live;
use client::{Client, Frame};

/// One gate item, run in its own folder.
type Item = fn(&Path) -> Result<Row>;

/// The argument the copied gate runs with.
const INNER: &str = "--inner";
/// How long the whole gate may run before it and its brains are killed.
const GATE_LIMIT: Duration = Duration::from_mins(20);
/// Prefixes of the markers an item expects gone and kept. FTS5 stores a term after the first on an
/// index page only as what it adds to the term before it, so the raw byte scan sees a whole marker
/// only when no other indexed term shares its first letter; no fixture word starts with these.
const GONE: &str = "qgone";
const KEPT: &str = "zkept";
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
    hermetic::require_gate(ProductIdentity::ACTIVE)?;
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

/// Builds the gate brain, copies it and this program into a fresh folder, runs the gate there
/// (every item, or only those whose names hold the filter), and then deletes what its brains left.
fn prepare(only: Option<&str>) -> Result<()> {
    let me = std::env::current_exe()?;
    let target = me.parent().context("xtask has a folder")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let folder = target
        .parent()
        .context("the profile folder has a parent")?
        .join("slice2-gate")
        .join(stamp.to_string());
    let run = Run::begin(folder.join(brain::DATA))?;
    let ran = (|| {
        let (debug, took) = hermetic::build(&["kumochi-brain"], Profile::Debug, None)?;
        println!("build_seconds={:.1}", took.as_secs_f64());
        let bin = folder.join("bin");
        fs::create_dir_all(&bin)?;
        fs::copy(debug.join(BRAIN_EXE), bin.join(BRAIN_EXE)).context("copying the brain")?;
        fs::copy(&me, bin.join(SHELL_EXE)).context("copying the client")?;
        println!("slice 2 gate in {}", folder.display());
        let mut gate = Command::new(bin.join(SHELL_EXE));
        gate.args(["slice2-gate", INNER]).arg(&folder).args(only);
        let ran = live::run_bounded(gate, GATE_LIMIT, "the gate")?;
        ensure!(ran.success(), "the gate failed");
        Ok(())
    })();
    run.finish(ran)
}

/// Ends each brain whose kill failed during its item and says how that went, so that item fails
/// and the next one doesn't meet it on the pipes. Only brains the gate started and still holds
/// are ended, so no other process can match, whatever its id.
fn end_leftovers(brains: Vec<Child>) -> Vec<String> {
    brains
        .into_iter()
        .map(|mut brain| {
            let pid = brain.id();
            match live::end(&mut brain) {
                Ok(()) => format!("brain {pid} outlived its kill; it has exited since"),
                Err(error) => format!("brain {pid} outlived its kill and wasn't ended: {error:#}"),
            }
        })
        .collect()
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
        let row = item(folder).unwrap_or_else(|error| Row {
            item: name,
            verdict: Verdict::Fail,
            detail: format!("{error:#}"),
        });
        let left = end_leftovers(brain::leftovers());
        rows.push(if left.is_empty() {
            row
        } else {
            Row::checked(name, &left, row.detail)
        });
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
        text: text.into(),
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

/// A random lowercase word starting with `prefix` that no fixture or store holds by chance.
fn marker(prefix: &str) -> String {
    use std::hash::{BuildHasher as _, RandomState};
    format!(
        "{prefix}{:016x}",
        RandomState::new().hash_one(SystemTime::now())
    )
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::{INNER, end_leftovers, percentile, run};

    /// A harmless child that waits on its stdin, which stays open while the command or child lives.
    fn held() -> Command {
        let mut command = Command::new("cmd");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn both_entries_refuse_unless_run_as_the_gate_identity() {
        let folder = crate::scratch::dir("slice2-refused");
        let inner = [
            INNER,
            folder.to_str().expect("Unicode"),
            "no item is named this",
        ];
        for args in [&inner[..], &[]] {
            let error = run(args.iter().map(|arg| (*arg).to_owned())).expect_err("refused");
            assert_eq!(
                format!("{error:#}"),
                "a gate tool runs only as the gate identity: cargo run --release --package xtask \
                 --features kumochi-protocol/gate-identity -- <task>"
            );
        }
        assert!(!folder.join("report.md").exists());
    }

    #[test]
    fn a_leftover_brain_is_ended_and_its_exit_confirmed() {
        let left = held().spawn().expect("cmd starts");
        let pid = left.id();
        let lines = end_leftovers(vec![left]);
        assert_eq!(
            lines,
            [format!(
                "brain {pid} outlived its kill; it has exited since"
            )]
        );
    }

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let samples: Vec<_> = (1..=21).rev().map(Duration::from_millis).collect();
        for (percent, milliseconds) in [(50, 11.0), (95, 20.0), (100, 21.0)] {
            let measured = percentile(&samples, percent);
            assert!(
                (measured - milliseconds).abs() < 1e-9,
                "p{percent}: {measured}"
            );
        }
        assert!(percentile(&[], 95).is_nan(), "no samples");
    }
}
