//! `cargo xtask live <check>`: the real dev brain, on the developer's own dev account, driven over
//! its real pipes by slice 2's gate client. `account` only reads the non-secret account view;
//! `signin` and `claude-signin` perform their respective sign-in flows, `claude-question` asks
//! Claude Code three questions, and `catalog-provenance`, `image` and `marks` run only the copied
//! brain, each in its own mode.
//!
//! The brain serves only the `kumochi.exe` beside it, so, as in the slice 2 gate, an outer xtask
//! builds the brain, copies it and itself (as `kumochi.exe`) into `target/live/<time>/bin`, and
//! runs that copy. Every wait is bounded, and a process that can't be ended is reported by pid.
#![expect(
    clippy::print_stdout,
    reason = "the harness's progress and the account's state are its output"
)]

mod catalog;
mod chatgpt_text;
mod claude_question;
mod claude_signin;
mod image;
mod marks;
mod signin;

use std::fs;
use std::io::{self, Read, Write};

use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{
    BRAIN_EXE, CLAUDE_QUESTIONS_ENV, IMAGE_CHECK_ARG, IMAGE_DIAGNOSTIC_ARG, LIVE_BUDGET_ENV,
    LIVE_RUN_BIN_DIR, LIVE_RUNS_DIR, ProductIdentity, SHELL_EXE,
};

use self::claude_signin::Ask;
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

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    match entry(ProductIdentity::ACTIVE, &args.collect::<Vec<_>>())? {
        Entry::Prepare(check) => prepare(check),
        Entry::Inner(Check::Account) => signin::harness(signin::inspect),
        Entry::Inner(Check::SignIn) => signin::inner(),
        Entry::Inner(Check::Claude(ask)) => claude_signin::inner(ask),
        Entry::Inner(Check::Question) => std::process::exit(claude_question::inner()),
        Entry::Inner(
            Check::Catalog
            | Check::Image
            | Check::ImageDiagnostic
            | Check::Marks
            | Check::ChatGptText(_),
        ) => {
            unreachable!("entry refuses --inner for the brain-only checks")
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Prepare(Check),
    /// The copied harness, which runs the brain beside its own executable.
    Inner(Check),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Account,
    SignIn,
    Claude(Ask),
    Question,
    Catalog,
    Image,
    ImageDiagnostic,
    Marks,
    ChatGptText(u32),
}

impl Check {
    const ALL: [Self; 9] = [
        Self::Account,
        Self::SignIn,
        Self::Claude(Ask::SignIn),
        Self::Claude(Ask::Disconnect),
        Self::Question,
        Self::Catalog,
        Self::Image,
        Self::ImageDiagnostic,
        Self::Marks,
    ];

    /// The arguments after `live` that name it.
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Account => &["account"],
            Self::SignIn => &["signin"],
            Self::Claude(Ask::SignIn) => &["claude-signin"],
            Self::Claude(Ask::Disconnect) => &["claude-signin", "--disconnect"],
            Self::Question => &["claude-question"],
            Self::Catalog => &["catalog-provenance"],
            Self::Image => &["image"],
            Self::ImageDiagnostic => &["image", "--diagnostic"],
            Self::Marks => &["marks"],
            Self::ChatGptText(_) => &["chatgpt-text"],
        }
    }
}

/// Which side of the harness the arguments after `live` ask for; both need the dev identity: a
/// release build uses the installed product's names, and the gate identity never signs in.
fn entry(identity: ProductIdentity, args: &[String]) -> Result<Entry> {
    ensure!(
        identity == ProductIdentity::DEV,
        "run it from a debug xtask without the gate feature (`cargo xtask live signin`): a release \
         one uses the installed product's pipe and credential names, and a gate one never signs in"
    );
    let (words, inner) = match args.split_last() {
        Some((last, words)) if last == INNER => (words, true),
        _ => (args, false),
    };
    let check = if let [command, options @ ..] = words
        && command == "chatgpt-text"
    {
        ensure!(!inner, "first-text is prepare-only");
        Check::ChatGptText(chatgpt_text::budget(options)?)
    } else {
        Check::ALL
        .into_iter()
        .find(|check| {
            words
                .iter()
                .map(String::as_str)
                .eq(check.args().iter().copied())
        })
        .filter(|check| {
            !(inner
                && matches!(
                    check,
                    Check::Catalog | Check::Image | Check::ImageDiagnostic | Check::Marks
                ))
        })
        .context(
            "usage: cargo xtask live account | signin | claude-signin [--disconnect] | claude-question | catalog-provenance | image [--diagnostic] | marks | chatgpt-text first-text --budget N",
        )?
    };
    Ok(if inner {
        Entry::Inner(check)
    } else {
        Entry::Prepare(check)
    })
}

/// Fails while a product process other than `me` runs: a dev brain holds the pipes the harness
/// uses and the account's session lock.
pub(crate) fn refuse_running(running: &[platform::Entry], me: u32) -> Result<()> {
    let others: Vec<String> = running
        .iter()
        .filter(|entry| entry.pid != me)
        .map(|entry| entry.pid.to_string())
        .collect();
    ensure!(
        others.is_empty(),
        "close every {SHELL_EXE} and {BRAIN_EXE} first (running: {})",
        others.join(", ")
    );
    Ok(())
}

/// Builds the dev brain, copies it and this program into a fresh folder, and runs the check there.
fn prepare(check: Check) -> Result<()> {
    let name = check.args().join(" ");
    refuse_running(&platform::processes()?, std::process::id())?;
    if check == Check::Question {
        claude_question::preflight(&platform::processes()?, std::process::id())?;
    }
    let text_plan = match check {
        Check::ChatGptText(budget) => Some(chatgpt_text::plan(budget)?),
        _ => None,
    };
    println!("live {name}: building the dev brain (at most {BUILD_LIMIT:?})");
    build_brain()?;
    let me = std::env::current_exe()?;
    let target = me.parent().context("xtask has a folder")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let live = target
        .parent()
        .context("the profile folder has a parent")?
        .join(LIVE_RUNS_DIR);
    let folder = live.join(stamp.to_string());
    let bin = folder.join(LIVE_RUN_BIN_DIR);
    if matches!(check, Check::ChatGptText(_)) {
        fs::create_dir_all(&live)?;
        fs::create_dir(&folder)?;
    }
    fs::create_dir_all(&bin)?;
    fs::copy(target.join(BRAIN_EXE), bin.join(BRAIN_EXE)).context("copying the brain")?;
    if text_plan.is_none() {
        println!("live {name}: running in {}", folder.display());
    }
    if check == Check::Catalog {
        return catalog::run(&folder);
    }
    match check {
        Check::Image => return image::run(&folder, IMAGE_CHECK_ARG),
        Check::ImageDiagnostic => return image::run(&folder, IMAGE_DIAGNOSTIC_ARG),
        Check::Marks => return marks::run(&folder),
        _ => {}
    }
    fs::copy(&me, bin.join(SHELL_EXE)).context("copying the client")?;
    if let Some(plan) = text_plan {
        chatgpt_text::prepared(&folder, &plan)?;
        println!("live {name}: prepared in {}", folder.display());
        return Ok(());
    }
    let mut inner = Command::new(bin.join(SHELL_EXE));
    inner.arg("live").args(check.args()).arg(INNER);
    if check == Check::Question {
        let ran = run_bounded(inner, claude_question::RUN_LIMIT, "the live run")
            .map(|status| status.code());
        let live = folder.parent().context("the run folder has a parent")?;
        return claude_question::report(ran, live, &mut io::stdout().lock());
    }
    let ran = run_bounded(inner, RUN_LIMIT, "the live run")?;
    ensure!(ran.success(), "live {name} failed ({ran})");
    Ok(())
}

/// Builds the dev brain, ending the build and everything it started past [`BUILD_LIMIT`].
fn build_brain() -> Result<()> {
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo.args(["build", "--quiet", "--package", "kumochi-brain"]);
    build(cargo, BUILD_LIMIT)
}

fn build(cargo: Command, limit: Duration) -> Result<()> {
    let built = run_bounded(cargo, limit, "the brain build")?;
    ensure!(built.success(), "the brain didn't build ({built})");
    Ok(())
}

/// Starts `command` and waits for it as [`bounded`] does.
pub(crate) fn run_bounded(mut command: Command, limit: Duration, what: &str) -> Result<ExitStatus> {
    let child = command
        .spawn()
        .with_context(|| format!("starting {what}"))?;
    bounded(child, limit, what)
}

/// Waits for `child` at most `limit`; past that it and everything it started are ended.
fn bounded(mut child: Child, limit: Duration, what: &str) -> Result<ExitStatus> {
    let failure = match wait_bounded(&mut child, limit) {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => anyhow!("{what} didn't finish in {limit:?}"),
        Err(error) => anyhow!("waiting for {what} failed: {error}"),
    };
    Err(joined(failure, end(&mut child)))
}

/// Waits for `child` as [`bounded`] does while a thread reads its stdout, so a full pipe never
/// blocks it, and returns its status, what it printed and how the read ended, as [`read_kept`].
fn drained(
    mut child: Child,
    limit: Duration,
    what: &str,
) -> Result<(Result<ExitStatus>, String, io::Result<()>)> {
    let output = child.stdout.take().context("the check's output")?;
    let reader = thread::spawn(move || read_kept(output));
    let status = bounded(child, limit, what);
    let (text, read) = reader
        .join()
        .unwrap_or_else(|_| (String::new(), Err(io::Error::other("the reader panicked"))));
    Ok((status, text, read))
}

/// Reads `output` to its end. A failed read keeps the complete lines read before it, so a check
/// still prints and counts the requests it sent.
fn read_kept(mut output: impl Read) -> (String, io::Result<()>) {
    let mut text = String::new();
    let read = output.read_to_string(&mut text).map(drop);
    if read.is_err() {
        text.truncate(text.rfind('\n').map_or(0, |end| end + 1));
    }
    (text, read)
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
pub(crate) fn end(child: &mut Child) -> Result<()> {
    let pid = child.id();
    let kill = platform::kill_tree(pid).map_err(|error| anyhow!("{error:#}; {}", survivor(pid)))?;
    end_with(child, kill, KILL_LIMIT)
}

fn survivor(pid: u32) -> String {
    format!("process {pid} may still be running; end it with `taskkill /T /F /PID {pid}`")
}

/// [`end`] with the kill command and its limit given. Only a root already gone before the kill
/// counts as ended: a failed or hung kill is an error even if the root exits meanwhile, since its
/// descendants may not have. Never waits on a child the kill didn't end.
fn end_with(child: &mut Child, mut kill: Command, limit: Duration) -> Result<()> {
    let pid = child.id();
    let fail = |problem: String| anyhow!("{problem}; {}", survivor(pid));
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => return Err(fail(format!("checking it failed ({error})"))),
    }
    let mut killer = kill
        .spawn()
        .map_err(|error| fail(format!("starting taskkill failed ({error})")))?;
    let killer_pid = killer.id();
    match wait_bounded(&mut killer, limit) {
        Ok(Some(status)) if status.success() => match wait_bounded(child, limit) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(fail(format!(
                "taskkill succeeded but the process didn't exit in {limit:?}"
            ))),
            Err(error) => Err(fail(format!("reaping it failed ({error})"))),
        },
        Ok(Some(status)) => Err(fail(format!("taskkill failed ({status})"))),
        Ok(None) | Err(_) => {
            let ended = killer
                .kill()
                .and_then(|()| wait_bounded(&mut killer, limit));
            let killer_state = if matches!(ended, Ok(Some(_))) {
                "was ended"
            } else {
                "may still be running"
            };
            Err(fail(format!(
                "taskkill (process {killer_pid}) didn't finish in {limit:?} and {killer_state}"
            )))
        }
    }
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
    /// Starts the brain beside this executable, with its model-request budget, its Claude Code
    /// question budget and no console output, inside a kill-on-close job this process holds, so
    /// the brain can't outlive it. Refuses while another product process runs.
    fn start(budget: u32, questions: u32) -> Result<Self> {
        let me = std::env::current_exe()?;
        let bin = me.parent().context("the harness has a folder")?;
        platform::Job::kill_on_close()
            .and_then(platform::Job::hold_self)
            .context("putting the run in a job")?;
        refuse_running(&platform::processes()?, std::process::id())?;
        let child = Command::new(bin.join(BRAIN_EXE))
            .env(LIVE_BUDGET_ENV, budget.to_string())
            .env(CLAUDE_QUESTIONS_ENV, questions.to_string())
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
    use std::io::{self, Read};
    use std::os::windows::process::CommandExt as _;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{
        Ask, Check, Entry, ProductIdentity, build, drained, end, end_with, entry, read_kept,
        refuse_running, run_bounded, wait_bounded, warn_cleanup, with_cleanup,
    };
    use crate::platform;

    const SHORT: Duration = Duration::from_millis(200);

    #[test]
    fn a_child_printing_far_more_than_a_pipe_holds_still_finishes() {
        let child = Command::new("cmd")
            .args([
                "/C",
                "for /L %i in (1,1,3000) do @echo request=%i status=2xx padding-padding-padding",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd starts");
        let (status, text, read) =
            drained(child, Duration::from_secs(30), "the test child").expect("drained");
        assert!(status.expect("it exits within the bound").success());
        read.expect("its output is read");
        assert!(text.len() > 64 * 1024, "{} bytes", text.len());
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("request="))
                .count(),
            3000
        );
    }

    #[test]
    fn a_read_that_fails_keeps_the_complete_lines_before_it() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("the pipe broke"))
            }
        }
        let output = &b"request=1 status=200\nrequest=2 status=200\nrequest=3 sta"[..];
        let (text, read) = read_kept(output.chain(Broken));
        assert_eq!(text, "request=1 status=200\nrequest=2 status=200\n");
        assert_eq!(
            read.expect_err("the failure is returned").to_string(),
            "the pipe broke"
        );
    }

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
    fn a_run_past_its_limit_is_ended_and_a_failed_one_returns_its_status() {
        let error = run_bounded(held(), SHORT, "the fixture").expect_err("times out");
        assert_eq!(format!("{error:#}"), "the fixture didn't finish in 200ms");
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
        let killer = held();
        let error = end_with(&mut child, killer, SHORT).expect_err("the kill hangs");
        let killer = error
            .to_string()
            .split_once("(process ")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(pid, _)| pid.to_owned())
            .expect("the killer's pid is named");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "taskkill (process {killer}) didn't finish in 200ms and was ended; process {pid} \
                 may still be running; end it with `taskkill /T /F /PID {pid}`"
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
        assert_eq!(format!("{only_run:#}"), "the run failed");
        let only_cleanup = with_cleanup(Ok(()), cleanup()).expect_err("fails");
        assert_eq!(
            format!("{only_cleanup:#}"),
            "process 7 may still be running"
        );
        let both = with_cleanup(failed(), cleanup()).expect_err("fails");
        assert_eq!(
            format!("{both:#}"),
            "the run failed; cleanup also failed: process 7 may still be running"
        );
    }

    #[test]
    fn a_failed_kill_stays_an_error_after_the_root_exits() {
        let mut child = held().spawn().expect("cmd starts");
        let pid = child.id();
        // Ends the root, as a partial tree kill would, then reports failure.
        let mut kill = Command::new("cmd");
        kill.arg("/C")
            .raw_arg(format!("taskkill /F /PID {pid} >nul & exit 1"));
        let error =
            end_with(&mut child, kill, Duration::from_secs(10)).expect_err("the kill failed");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "taskkill failed (exit code: 1); process {pid} may still be running; end it with \
                 `taskkill /T /F /PID {pid}`"
            )
        );
        child.wait().expect("the root is reaped");
    }

    #[test]
    fn both_sides_refuse_a_release_build_and_take_no_folder() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let release = "run it from a debug xtask without the gate feature (`cargo xtask live \
                       signin`): a release one uses the installed product's pipe and credential \
                       names, and a gate one never signs in";
        for list in [
            &["account"][..],
            &["signin"][..],
            &["signin", "--inner"],
            &["claude-signin", "--disconnect", "--inner"],
            &["claude-question"],
            &["claude-question", "--inner"],
            &["catalog-provenance"],
        ] {
            for identity in [ProductIdentity::RELEASE, ProductIdentity::GATE] {
                let error = entry(identity, &args(list)).expect_err("not dev");
                assert_eq!(format!("{error:#}"), release);
            }
        }
        let claude = Check::Claude;
        for (list, want) in [
            (&["account"][..], Entry::Prepare(Check::Account)),
            (&["account", "--inner"], Entry::Inner(Check::Account)),
            (&["signin"][..], Entry::Prepare(Check::SignIn)),
            (&["signin", "--inner"], Entry::Inner(Check::SignIn)),
            (&["claude-signin"], Entry::Prepare(claude(Ask::SignIn))),
            (
                &["claude-signin", "--inner"],
                Entry::Inner(claude(Ask::SignIn)),
            ),
            (
                &["claude-signin", "--disconnect"],
                Entry::Prepare(claude(Ask::Disconnect)),
            ),
            (
                &["claude-signin", "--disconnect", "--inner"],
                Entry::Inner(claude(Ask::Disconnect)),
            ),
            (&["claude-question"], Entry::Prepare(Check::Question)),
            (
                &["claude-question", "--inner"],
                Entry::Inner(Check::Question),
            ),
            (&["catalog-provenance"], Entry::Prepare(Check::Catalog)),
            (&["marks"], Entry::Prepare(Check::Marks)),
            (&["image"], Entry::Prepare(Check::Image)),
            (
                &["image", "--diagnostic"],
                Entry::Prepare(Check::ImageDiagnostic),
            ),
        ] {
            assert_eq!(
                entry(ProductIdentity::DEV, &args(list)).expect("debug"),
                want
            );
        }
        for list in [
            &[][..],
            &["--inner"],
            &["signin", "--inner", "C:/elsewhere"],
            &["signin", "C:/elsewhere"],
            &["signin", "--disconnect"],
            &["claude-signin", "--inner", "--disconnect"],
            &["claude-signin", "--disconnect", "--disconnect"],
            &["--disconnect"],
            &["first-text"],
            &["catalog-provenance", "--inner"],
            &["catalog-provenance", "C:/elsewhere"],
            &["image", "--inner"],
            &["image", "C:/elsewhere"],
            &["image", "--diagnostic", "--inner"],
            &["--diagnostic"],
            &["marks", "--inner"],
            &["marks", "--dry"],
            &["claude-question", "--disconnect"],
            &["claude-question", "3"],
            &["claude-question", "--inner", "--inner"],
        ] {
            let error = entry(ProductIdentity::DEV, &args(list)).expect_err("refused");
            assert_eq!(
                format!("{error:#}"),
                "usage: cargo xtask live account | signin | claude-signin [--disconnect] | claude-question | catalog-provenance | image [--diagnostic] | marks | chatgpt-text first-text --budget N"
            );
        }
    }

    #[test]
    fn only_the_harness_itself_may_run_at_launch() {
        let entry = |pid, shell| platform::Entry {
            pid,
            parent: 1,
            shell,
        };
        refuse_running(&[], 7).expect("nothing runs");
        refuse_running(&[entry(7, true)], 7).expect("the copied harness is itself");
        let error = refuse_running(&[entry(7, true), entry(9, false), entry(11, true)], 7)
            .expect_err("a brain and a shell run");
        assert_eq!(
            format!("{error:#}"),
            "close every kumochi.exe and kumochid.exe first (running: 9, 11)"
        );
    }
}
