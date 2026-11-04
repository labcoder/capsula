//! `cargo xtask live <check>`: the real dev brain, on the developer's own dev account, driven over
//! its real pipes by slice 2's gate client. The only check so far is `signin`.
//!
//! The brain serves only the `kumochi.exe` beside it, so, as in the slice 2 gate, an outer xtask
//! builds the brain, copies it and itself (as `kumochi.exe`) into `target/live/<time>/bin`, and
//! runs that copy. Every wait is bounded, and a process that can't be ended is reported by pid.
#![expect(
    clippy::print_stdout,
    reason = "the harness's progress and the account's state are its output"
)]

mod signin;

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, LIVE_BUDGET_ENV, SHELL_EXE};

use crate::platform;

/// The argument the copied harness runs with.
const INNER: &str = "--inner";
/// How long building the brain may take [guess: a cold build on Rez takes a few minutes].
const BUILD_LIMIT: Duration = Duration::from_mins(20);
/// How long the copied harness may run; the final bound on everything it waits for.
const RUN_LIMIT: Duration = Duration::from_mins(8);
/// How long `taskkill` gets, and then the process it ended, to be gone.
const KILL_LIMIT: Duration = Duration::from_secs(10);
/// How long the brain gets to exit after `Quitting`: its quit takes 2 s plus at most 5 s for a
/// refresh already out.
const EXIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    ensure!(
        args.next().as_deref() == Some("signin"),
        "usage: cargo xtask live signin"
    );
    match (args.next(), args.next(), args.next()) {
        (None, _, _) => prepare(),
        (Some(flag), Some(folder), None) if flag == INNER => {
            signin::inner(&Path::new(&folder).join("bin"))
        }
        _ => bail!("live signin takes no arguments"),
    }
}

/// Builds the dev brain, copies it and this program into a fresh folder, and runs the check there.
fn prepare() -> Result<()> {
    ensure!(
        cfg!(debug_assertions),
        "run it from a debug xtask (`cargo xtask live signin`): a release one would copy a \
         release brain, which uses the installed product's pipe and credential names"
    );
    ensure!(
        platform::processes()?.is_empty(),
        "close every {SHELL_EXE} and {BRAIN_EXE} first: a dev brain holds the pipes the harness \
         uses and the account's session lock"
    );
    println!("live signin: building the dev brain (at most {BUILD_LIMIT:?})");
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo.args(["build", "--quiet", "--package", "kumochi-brain"]);
    let built = run_bounded(cargo, BUILD_LIMIT, "the brain build")?;
    ensure!(built.success(), "the brain didn't build ({built})");
    let me = std::env::current_exe()?;
    let target = me.parent().context("xtask has a folder")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let folder = target
        .parent()
        .context("the profile folder has a parent")?
        .join("live")
        .join(stamp.to_string());
    let bin = folder.join("bin");
    fs::create_dir_all(&bin)?;
    fs::copy(target.join(BRAIN_EXE), bin.join(BRAIN_EXE)).context("copying the brain")?;
    fs::copy(&me, bin.join(SHELL_EXE)).context("copying the client")?;
    println!("live signin: running in {}", folder.display());
    let mut inner = Command::new(bin.join(SHELL_EXE));
    inner.args(["live", "signin", INNER]).arg(&folder);
    let ran = run_bounded(inner, RUN_LIMIT, "the live run")?;
    ensure!(ran.success(), "live signin failed ({ran})");
    Ok(())
}

/// Runs `command` for at most `limit`; past that it and everything it started are ended.
fn run_bounded(mut command: Command, limit: Duration, what: &str) -> Result<ExitStatus> {
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {what}"))?;
    let failure = match wait_bounded(&mut child, limit) {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => anyhow!("{what} didn't finish in {limit:?}"),
        Err(error) => anyhow!("waiting for {what} failed: {error}"),
    };
    Err(joined(failure, end(&mut child)))
}

/// Waits for `child` at most `limit`; `None` while it still runs.
fn wait_bounded(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let end = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        thread::sleep(left.min(POLL));
    }
}

/// Ends `child` and everything it started, and reaps it.
fn end(child: &mut Child) -> Result<()> {
    end_with(child, platform::kill_tree(child.id())?, KILL_LIMIT)
}

/// [`end`] with the kill command and its limit given. Never waits on a child the kill didn't end;
/// an error names every process that may still run.
fn end_with(child: &mut Child, mut kill: Command, limit: Duration) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let pid = child.id();
    let survivor =
        || format!("process {pid} may still be running; end it with `taskkill /T /F /PID {pid}`");
    let mut killer = kill
        .spawn()
        .with_context(|| format!("starting taskkill failed; {}", survivor()))?;
    let problem = match wait_bounded(&mut killer, limit) {
        Ok(Some(status)) if status.success() => {
            if wait_bounded(child, limit)?.is_some() {
                return Ok(());
            }
            format!("taskkill succeeded but the process didn't exit in {limit:?}")
        }
        Ok(Some(status)) => format!("taskkill failed ({status})"),
        Ok(None) | Err(_) => match killer
            .kill()
            .and_then(|()| wait_bounded(&mut killer, limit))
        {
            Ok(Some(_)) => format!("taskkill didn't finish in {limit:?}"),
            _ => format!(
                "taskkill (process {}) didn't finish in {limit:?} and couldn't be ended",
                killer.id()
            ),
        },
    };
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    bail!("{problem}; {}", survivor())
}

/// A failure, with a cleanup failure after it if there was one.
fn joined(failure: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => failure,
        Err(cleanup) => anyhow!("{failure:#}; cleanup also failed: {cleanup:#}"),
    }
}

/// `result`, unless the cleanup after it failed too.
fn with_cleanup(result: Result<()>, cleanup: Result<()>) -> Result<()> {
    match result {
        Ok(()) => cleanup,
        Err(failure) => Err(joined(failure, cleanup)),
    }
}

/// Reports a cleanup that failed where nothing can return it.
fn warn_cleanup(stderr: &mut impl Write, cleanup: Result<()>) {
    if let Err(error) = cleanup {
        let _best_effort = writeln!(stderr, "live: couldn't clean up: {error:#}");
    }
}

/// The dev brain a check started. Dropping it ends it, and a failure to is written to stderr.
#[derive(Debug)]
struct Brain {
    child: Child,
}

impl Brain {
    /// Starts the brain beside the harness, with its model-request budget and no console output.
    fn start(bin: &Path, budget: u32) -> Result<Self> {
        let child = Command::new(bin.join(BRAIN_EXE))
            .env(LIVE_BUDGET_ENV, budget.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the brain")?;
        Ok(Self { child })
    }

    /// After `Quitting` was sent, waits for the brain to exit on its own; otherwise ends it.
    fn quit(mut self) -> Result<()> {
        match wait_bounded(&mut self.child, EXIT)? {
            Some(status) if status.success() => Ok(()),
            Some(status) => bail!("the brain exited with {status}"),
            None => Err(joined(
                anyhow!("the brain didn't exit in {EXIT:?} after Quitting; it was ended"),
                end(&mut self.child),
            )),
        }
    }

    /// Ends a brain that wasn't told to quit.
    fn stop(mut self) -> Result<()> {
        end(&mut self.child)
    }
}

impl Drop for Brain {
    fn drop(&mut self) {
        warn_cleanup(&mut io::stderr().lock(), end(&mut self.child));
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{end, end_with, run_bounded, wait_bounded, warn_cleanup, with_cleanup};

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

    fn exits(code: u8) -> Command {
        let mut command = Command::new("cmd");
        command.args(["/C", &format!("exit {code}")]);
        command
    }

    fn reap(mut child: Child) {
        child.kill().expect("the test child can be killed");
        child.wait().expect("the test child is reaped");
    }

    #[test]
    fn a_bounded_wait_returns_the_status_or_none_at_its_limit() {
        let mut quick = exits(3).spawn().expect("cmd starts");
        let status = wait_bounded(&mut quick, Duration::from_secs(30)).expect("waits");
        assert_eq!(status.and_then(|status| status.code()), Some(3));

        let mut slow = held().spawn().expect("cmd starts");
        let start = Instant::now();
        assert_eq!(wait_bounded(&mut slow, SHORT).expect("waits"), None);
        assert!(
            start.elapsed() >= SHORT,
            "returned early: {:?}",
            start.elapsed()
        );
        end(&mut slow).expect("taskkill ends a held cmd");
        assert!(
            slow.try_wait().expect("reads").is_some(),
            "the child is reaped"
        );
    }

    #[test]
    fn a_run_past_its_limit_is_ended_and_a_failed_one_returns_its_status() {
        let error = run_bounded(held(), SHORT, "the fixture").expect_err("times out");
        assert_eq!(format!("{error:?}"), "the fixture didn't finish in 200ms");
        let status = run_bounded(exits(3), SHORT * 50, "the fixture").expect("finishes");
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn a_failed_kill_names_the_survivor_on_stderr() {
        let mut child = held().spawn().expect("cmd starts");
        let pid = child.id();
        let error = end_with(&mut child, exits(1), SHORT).expect_err("the kill fails");
        let mut stderr = Vec::new();
        warn_cleanup(&mut stderr, Err(error));
        assert_eq!(
            String::from_utf8(stderr).expect("utf-8"),
            format!(
                "live: couldn't clean up: taskkill failed (exit code: 1); process {pid} may still \
                 be running; end it with `taskkill /T /F /PID {pid}`\n"
            )
        );
        reap(child);
    }

    #[test]
    fn a_hung_kill_is_ended_and_the_survivor_named() {
        let mut child = held().spawn().expect("cmd starts");
        let pid = child.id();
        let error = end_with(&mut child, held(), SHORT).expect_err("the kill hangs");
        assert_eq!(
            format!("{error:?}"),
            format!(
                "taskkill didn't finish in 200ms; process {pid} may still be running; end it \
                 with `taskkill /T /F /PID {pid}`"
            )
        );
        reap(child);
    }

    #[test]
    fn a_cleanup_failure_joins_the_first_failure_without_hiding_it() {
        let failed = || Err(anyhow::anyhow!("the run failed"));
        let cleanup = || Err(anyhow::anyhow!("process 7 may still be running"));
        assert!(with_cleanup(Ok(()), Ok(())).is_ok());
        let only_run = with_cleanup(failed(), Ok(())).expect_err("fails");
        assert_eq!(format!("{only_run:?}"), "the run failed");
        let only_cleanup = with_cleanup(Ok(()), cleanup()).expect_err("fails");
        assert_eq!(
            format!("{only_cleanup:?}"),
            "process 7 may still be running"
        );
        let both = with_cleanup(failed(), cleanup()).expect_err("fails");
        assert_eq!(
            format!("{both:?}"),
            "the run failed; cleanup also failed: process 7 may still be running"
        );
    }
}
