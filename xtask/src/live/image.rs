//! `cargo xtask live image`: runs the copied dev brain once in its image check mode, bounded and
//! in a kill-on-close job, and prints the brain's request lines and one line from its exit code.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, IMAGE_CHECK_ARG};

use super::{bounded, refuse_running};
use crate::platform;

/// How long the check may run [guess: three requests, each under the stream's 10 s send and
/// 60 s idle bounds, plus building a 3 MB image a few times].
const LIMIT: Duration = Duration::from_mins(6);

/// Runs the brain in `folder`'s `bin` and reports it. Any exit but 0 fails the command.
pub(super) fn run(folder: &Path) -> Result<()> {
    platform::Job::kill_on_close()
        .and_then(platform::Job::hold_self)
        .context("putting the run in a job")?;
    refuse_running(&platform::processes()?, std::process::id())?;
    let mut child = Command::new(folder.join("bin").join(BRAIN_EXE))
        .arg(IMAGE_CHECK_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the image check (requests: 0)")?;
    let mut output = child.stdout.take().context("the check's output")?;
    let status = bounded(child, LIMIT, "the image check");
    let mut text = String::new();
    let read = output.read_to_string(&mut text);
    let lines = request_lines(&text);
    for line in &lines {
        println!("{line}");
    }
    println!(
        "{}",
        summary(status.as_ref().ok().and_then(ExitStatus::code), lines.len())
    );
    read.context("reading the check's lines")?;
    ensure!(status?.success(), "the image check stopped");
    Ok(())
}

/// The brain's request lines; anything else it printed is dropped.
fn request_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with("request="))
        .collect()
}

/// The run's line, from its exit code and the number of request lines. A run that ended without
/// a code may have had one more request in flight.
fn summary(code: Option<i32>, lines: usize) -> String {
    match code {
        Some(0) => format!("image check: done (requests: {lines})"),
        Some(code @ 10..=19) => format!("image check: nothing sent, exit {code} (requests: 0)"),
        Some(code) => format!("image check: stopped, exit {code} (requests: {lines})"),
        None => format!(
            "image check: no exit code (requests: at most {})",
            lines + 1
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{request_lines, summary};

    #[test]
    fn only_request_lines_are_kept() {
        assert_eq!(
            request_lines("noise\nrequest=1 image=small\r\n request=2\nrequest=3 x\n"),
            ["request=1 image=small", "request=3 x"]
        );
        assert!(request_lines("").is_empty());
    }

    #[test]
    fn each_line_counts_what_the_exit_code_allows() {
        for (code, lines, line) in [
            (Some(0), 3, "image check: done (requests: 3)"),
            (
                Some(10),
                0,
                "image check: nothing sent, exit 10 (requests: 0)",
            ),
            (
                Some(19),
                0,
                "image check: nothing sent, exit 19 (requests: 0)",
            ),
            (Some(20), 2, "image check: stopped, exit 20 (requests: 2)"),
            (Some(-1), 1, "image check: stopped, exit -1 (requests: 1)"),
            (None, 2, "image check: no exit code (requests: at most 3)"),
        ] {
            assert_eq!(summary(code, lines), line);
        }
    }
}
