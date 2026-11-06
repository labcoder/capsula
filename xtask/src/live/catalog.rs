//! `cargo xtask live catalog-provenance`: runs the copied dev brain once in its capture mode,
//! bounded and in a kill-on-close job, and prints one line that counts the requests it sent.

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, CATALOG_PROVENANCE_ARG, CATALOG_PROVENANCE_FILE};

use super::{bounded, refuse_running};
use crate::platform;

/// How long the capture may run [guess: its 120 s request plus the vault open and trust store].
const LIMIT: Duration = Duration::from_mins(3);

/// Runs the brain in `folder`'s `bin` and reports it. Any exit but 0 fails the command.
pub(super) fn run(folder: &Path) -> Result<()> {
    platform::Job::kill_on_close()
        .and_then(platform::Job::hold_self)
        .context("putting the run in a job")?;
    refuse_running(&platform::processes()?, std::process::id())?;
    let child = Command::new(folder.join("bin").join(BRAIN_EXE))
        .arg(CATALOG_PROVENANCE_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the capture (requests: auth 0, catalog 0)")?;
    let status = bounded(child, LIMIT, "the capture");
    let code = status.as_ref().ok().and_then(ExitStatus::code);
    println!("{}", report(code, &folder.join(CATALOG_PROVENANCE_FILE)));
    ensure!(status?.success(), "the capture didn't write its file");
    Ok(())
}

/// The run's line, from its exit code and fixed words only. 10–19 sent nothing; any other end,
/// or none seen, counts as the one catalog request.
fn report(code: Option<i32>, file: &Path) -> String {
    match code {
        Some(0) => format!(
            "catalog provenance: written: {} (requests: auth 0, catalog 1)",
            file.display()
        ),
        Some(code @ 10..=19) => {
            format!("catalog provenance: not written, exit {code} (requests: auth 0, catalog 0)")
        }
        Some(code) => {
            format!("catalog provenance: not written, exit {code} (requests: auth 0, catalog 1)")
        }
        None => {
            "catalog provenance: no exit code (requests: auth 0, counted as catalog 1)".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::report;

    #[test]
    fn each_line_counts_what_the_exit_code_allows() {
        let file = Path::new(r"C:\repo\target\live\1\catalog-provenance.json");
        assert_eq!(
            report(Some(0), file),
            r"catalog provenance: written: C:\repo\target\live\1\catalog-provenance.json (requests: auth 0, catalog 1)"
        );
        for (code, line) in [
            (
                10,
                "catalog provenance: not written, exit 10 (requests: auth 0, catalog 0)",
            ),
            (
                19,
                "catalog provenance: not written, exit 19 (requests: auth 0, catalog 0)",
            ),
            (
                20,
                "catalog provenance: not written, exit 20 (requests: auth 0, catalog 1)",
            ),
            (
                29,
                "catalog provenance: not written, exit 29 (requests: auth 0, catalog 1)",
            ),
            (
                30,
                "catalog provenance: not written, exit 30 (requests: auth 0, catalog 1)",
            ),
            (
                -1,
                "catalog provenance: not written, exit -1 (requests: auth 0, catalog 1)",
            ),
        ] {
            assert_eq!(report(Some(code), file), line);
        }
        assert_eq!(
            report(None, file),
            "catalog provenance: no exit code (requests: auth 0, counted as catalog 1)"
        );
    }
}
