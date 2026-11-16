//! `cargo xtask live marks`: runs the copied dev brain once in its screen marks check mode,
//! bounded and in a kill-on-close job, and prints the brain's lines and one line from its exit
//! code. `KUMOCHI_LIVE_BUDGET` reaches the brain through the environment; 0 is the dry run.

use std::io::{self, Read as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, MARKS_CHECK_ARG};

use super::{bounded, refuse_running};
use crate::platform;

/// How long the check may run [guess: 37 requests at well under 45 s each].
const LIMIT: Duration = Duration::from_mins(30);
/// The brain's lines kept; anything else it printed is dropped.
const KEPT: [&str; 4] = ["marks-check ", "request=", "mark=", "summary "];

pub(super) fn run(folder: &Path) -> Result<()> {
    platform::Job::kill_on_close()
        .and_then(platform::Job::hold_self)
        .context("putting the run in a job")?;
    refuse_running(&platform::processes()?, std::process::id())?;
    let child = Command::new(folder.join("bin").join(BRAIN_EXE))
        .arg(MARKS_CHECK_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the marks check (requests: 0)")?;
    let (status, text) = drained(child, LIMIT, "the marks check")?;
    let text = text.context("reading the check's lines");
    let lines = text.as_deref().map(kept).unwrap_or_default();
    for line in &lines {
        println!("{line}");
    }
    println!(
        "{}",
        summary(status.as_ref().ok().and_then(ExitStatus::code), &lines)
    );
    text?;
    ensure!(status?.success(), "the marks check stopped");
    Ok(())
}

/// Waits for `child` as [`bounded`] does while a thread reads its stdout, so a full pipe never
/// blocks it, and returns its status and everything it printed.
fn drained(
    mut child: Child,
    limit: Duration,
    what: &str,
) -> Result<(Result<ExitStatus>, io::Result<String>)> {
    let mut output = child.stdout.take().context("the check's output")?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        output.read_to_string(&mut text).map(|_| text)
    });
    let status = bounded(child, limit, what);
    let text = reader
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the reader panicked")));
    Ok((status, text))
}

fn kept(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| KEPT.iter().any(|start| line.starts_with(start)))
        .collect()
}

/// The run's line. Requests that went out are the request lines with a status; a run that stopped
/// may have sent one more whose line never printed.
fn summary(code: Option<i32>, lines: &[&str]) -> String {
    let sent = lines
        .iter()
        .filter(|line| line.starts_with("request=") && line.contains(" status="))
        .count();
    match code {
        Some(0) => format!("marks check: done (requests: {sent})"),
        Some(code @ 10..=19) => format!("marks check: nothing sent, exit {code} (requests: 0)"),
        Some(code) => format!(
            "marks check: stopped, exit {code} (requests: at most {})",
            sent + 1
        ),
        None => format!("marks check: no exit code (requests: at most {})", sent + 1),
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::{drained, kept, summary};

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
        let (status, text) =
            drained(child, Duration::from_secs(30), "the test child").expect("drained");
        assert!(status.expect("it exits within the bound").success());
        let text = text.expect("its output is read");
        assert!(text.len() > 64 * 1024, "{} bytes", text.len());
        assert_eq!(kept(&text).len(), 3000);
    }

    #[test]
    fn only_the_check_s_lines_are_kept() {
        assert_eq!(
            kept(
                "noise\nmarks-check model=m\nrequest=1 x\n mark=1.1\nmark=1.1 y\nsummary sent=1\nsummary\n"
            ),
            [
                "marks-check model=m",
                "request=1 x",
                "mark=1.1 y",
                "summary sent=1"
            ]
        );
        assert!(kept("").is_empty());
    }

    #[test]
    fn each_line_counts_only_requests_that_went_out() {
        let lines = [
            "request=1 layout=L1 status=2xx",
            "mark=1.1 thing=button",
            "request=2 layout=L1 result=skipped reason=size-refused",
            "request=3 layout=L1 result=dry",
            "request=4 layout=L2 status=400",
        ];
        for (code, line) in [
            (Some(0), "marks check: done (requests: 2)"),
            (Some(10), "marks check: nothing sent, exit 10 (requests: 0)"),
            (Some(19), "marks check: nothing sent, exit 19 (requests: 0)"),
            (
                Some(20),
                "marks check: stopped, exit 20 (requests: at most 3)",
            ),
            (None, "marks check: no exit code (requests: at most 3)"),
        ] {
            assert_eq!(summary(code, &lines), line);
        }
        assert_eq!(summary(Some(0), &[]), "marks check: done (requests: 0)");
    }
}
