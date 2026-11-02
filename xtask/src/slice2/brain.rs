//! The real brain, started from the gate's folder with its own `%LOCALAPPDATA%` and fixture.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use capsula_protocol::constants::{BRAIN_EXE, DATA_DIR, FIXTURE_ENV, STORE_FILE};

/// How long a quitting brain gets to exit.
const EXIT: Duration = Duration::from_secs(15);

/// One gate item's folder: its fixture script, and the `%LOCALAPPDATA%` its brains share.
#[derive(Debug)]
pub(super) struct Case {
    bin: PathBuf,
    fixture: PathBuf,
    /// What the brain sees as `%LOCALAPPDATA%`: its store and logs, and nothing else.
    pub(super) local: PathBuf,
}

impl Case {
    pub(super) fn new(folder: &Path, name: &str, script: &str) -> Result<Self> {
        let root = folder.join(name);
        let local = root.join("local");
        fs::create_dir_all(&local).with_context(|| format!("creating {}", local.display()))?;
        let case = Self {
            bin: folder.join("bin"),
            fixture: root.join("fixture.txt"),
            local,
        };
        case.script(script)?;
        Ok(case)
    }

    /// Replaces the script the next brain plays.
    pub(super) fn script(&self, script: &str) -> Result<()> {
        fs::write(&self.fixture, script).context("writing the fixture")
    }

    /// The brain's data folder.
    pub(super) fn data(&self) -> PathBuf {
        self.local.join(DATA_DIR)
    }

    pub(super) fn store(&self) -> PathBuf {
        self.data().join(STORE_FILE)
    }

    pub(super) fn start(&self) -> Result<Brain> {
        let child = Command::new(self.bin.join(BRAIN_EXE))
            .env("LOCALAPPDATA", &self.local)
            .env(FIXTURE_ENV, &self.fixture)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the brain")?;
        Ok(Brain { child })
    }
}

/// A running brain; dropping it kills it.
#[derive(Debug)]
pub(super) struct Brain {
    child: Child,
}

impl Brain {
    /// Ends it with `TerminateProcess`, as a crash would.
    pub(super) fn kill(mut self) -> Result<()> {
        self.child.kill().context("killing the brain")?;
        self.child.wait()?;
        Ok(())
    }

    /// Waits for it to exit on its own, as it does after `Quitting`.
    pub(super) fn exited(mut self) -> Result<()> {
        let end = Instant::now() + EXIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                if !status.success() {
                    bail!("the brain exited with {status}");
                }
                return Ok(());
            }
            if Instant::now() >= end {
                bail!("the brain didn't exit in {EXIT:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Brain {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _killed = self.child.kill();
            let _reaped = self.child.wait();
        }
    }
}
