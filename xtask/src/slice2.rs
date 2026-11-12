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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, SHELL_EXE};
use kumochi_protocol::message::{ControlToShell, DataToBrain, DataToShell, Phase, RunId, ThreadId};

use crate::{live, platform};
use client::{Client, Frame};

/// One gate item, run in its own folder.
type Item = fn(&Path) -> Result<Row>;

/// The argument the copied gate runs with.
const INNER: &str = "--inner";
/// How long building the brain may take before the build and everything it started are killed
/// [guess: a cold build on Rez takes a few minutes].
const BUILD_LIMIT: Duration = Duration::from_mins(20);
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
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo.args(["build", "--quiet", "--package", "kumochi-brain"]);
    build(cargo, BUILD_LIMIT)?;
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
    let mut gate = Command::new(bin.join(SHELL_EXE));
    gate.args(["slice2-gate", INNER]).arg(&folder).args(only);
    let ran = live::run_bounded(gate, GATE_LIMIT, "the gate")?;
    ensure!(ran.success(), "the gate failed");
    Ok(())
}

/// Runs the brain build, ending it and everything it started past `limit`.
fn build(cargo: Command, limit: Duration) -> Result<()> {
    let built = live::run_bounded(cargo, limit, "the brain build")?;
    ensure!(built.success(), "the brain didn't build ({built})");
    Ok(())
}

/// Ends each brain still running after its item and says whether taskkill ended it, so the item
/// that left it fails, and the next one doesn't meet it on the pipes.
fn end_survivors(pids: impl IntoIterator<Item = u32>) -> Vec<String> {
    pids.into_iter()
        .map(|pid| {
            let ended = platform::kill_tree(pid)
                .and_then(|kill| live::run_bounded(kill, live::KILL_LIMIT, "taskkill"));
            match ended {
                Ok(status) if status.success() => {
                    format!("brain {pid} outlived the item; taskkill ended it")
                }
                Ok(status) => format!("brain {pid} outlived the item; taskkill failed ({status})"),
                Err(error) => format!("brain {pid} outlived the item; taskkill failed: {error:#}"),
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
        let survivors = end_survivors(
            platform::processes()?
                .into_iter()
                .filter(|entry| entry.parent == std::process::id())
                .map(|entry| entry.pid),
        );
        rows.push(if survivors.is_empty() {
            row
        } else {
            Row::checked(name, &survivors, row.detail)
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
    use std::time::{Duration, Instant};

    use super::{GONE, KEPT, build, end_survivors, marker, percentile};

    const SHORT: Duration = Duration::from_millis(200);

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
    fn a_build_past_its_limit_is_ended() {
        let start = Instant::now();
        let error = build(held(), SHORT).expect_err("a hung build fails");
        assert_eq!(
            format!("{error:#}"),
            "the brain build didn't finish in 200ms"
        );
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "it returned in bound"
        );
    }

    #[test]
    fn a_brain_left_by_its_item_is_ended_and_a_failed_kill_is_reported() {
        let mut left = held().spawn().expect("cmd starts");
        let mut exited = Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("cmd starts");
        exited.wait().expect("it exits");
        // The open handle keeps the exited process's id from being reused, so taskkill can't find it.
        let lines = end_survivors([left.id(), exited.id()]);
        assert_eq!(
            lines,
            [
                format!("brain {} outlived the item; taskkill ended it", left.id()),
                format!(
                    "brain {} outlived the item; taskkill failed (exit code: 128)",
                    exited.id()
                ),
            ]
        );
        let start = Instant::now();
        while left.try_wait().expect("reads").is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the survivor outlived taskkill"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_gone_and_kept_markers_share_no_prefix() {
        let (gone, kept) = (marker(GONE), marker(KEPT));
        let shared = gone
            .bytes()
            .zip(kept.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert_eq!(shared, 0, "{gone} and {kept}");
    }

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let samples: Vec<_> = (1..=20).rev().map(Duration::from_millis).collect();
        assert!((percentile(&samples, 50) - 10.0).abs() < 1e-9, "p50");
        assert!((percentile(&samples, 95) - 19.0).abs() < 1e-9, "p95");
        assert!((percentile(&samples, 100) - 20.0).abs() < 1e-9, "max");
        assert!(percentile(&[], 95).is_nan(), "no samples");
    }
}
