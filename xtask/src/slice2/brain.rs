//! The real brain, started from the gate's folder with its own `%LOCALAPPDATA%`, `%APPDATA%` and
//! fixture.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kumochi_protocol::constants::{BRAIN_EXE, DATA_DIR, FIXTURE_ENV, STORE_FILE};

use crate::hermetic::Folders;

/// How long a quitting brain gets to exit.
const EXIT: Duration = Duration::from_secs(15);

/// Brains whose kill failed, still held so their ids can't be reused; the gate ends them after
/// the item that left them, and fails that item.
static LEFTOVERS: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Takes the brains left since the last call.
pub(super) fn leftovers() -> Vec<Child> {
    std::mem::take(&mut *LEFTOVERS.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The folder under the gate's folder that holds the items' folders, which the run deletes.
pub(super) const DATA: &str = "data";

/// One gate item's folder: its fixture script, and the `%LOCALAPPDATA%` and `%APPDATA%` its
/// brains share.
#[derive(Debug)]
pub(super) struct Case {
    bin: PathBuf,
    fixture: PathBuf,
    /// What the brain sees as `%LOCALAPPDATA%` (its store and logs) and `%APPDATA%`.
    pub(super) folders: Folders,
}

impl Case {
    pub(super) fn new(folder: &Path, name: &str, script: &str) -> Result<Self> {
        let root = folder.join(DATA).join(name);
        let case = Self {
            bin: folder.join("bin"),
            fixture: root.join("fixture.txt"),
            folders: Folders::create(&root)?,
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
        self.folders.local.join(DATA_DIR)
    }

    pub(super) fn store(&self) -> PathBuf {
        self.data().join(STORE_FILE)
    }

    pub(super) fn start(&self) -> Result<Brain> {
        let child = self
            .folders
            .apply(&mut Command::new(self.bin.join(BRAIN_EXE)))
            .env(FIXTURE_ENV, &self.fixture)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the brain")?;
        Ok(Brain { child: Some(child) })
    }
}

/// A running brain; dropping it kills it, or leaves it to the gate when the kill fails.
#[derive(Debug)]
pub(super) struct Brain {
    /// Taken only by `kill` and `drop`.
    child: Option<Child>,
}

impl Brain {
    /// Ends it with `TerminateProcess`, as a crash would.
    pub(super) fn kill(mut self) -> Result<()> {
        let mut child = self.child.take().context("the brain was already taken")?;
        if let Err(error) = child.kill() {
            keep(child);
            return Err(error).context("killing the brain");
        }
        child.wait()?;
        Ok(())
    }

    /// Waits for it to exit on its own, as it does after `Quitting`.
    pub(super) fn exited(mut self) -> Result<()> {
        let child = self.child.as_mut().context("the brain was already taken")?;
        let end = Instant::now() + EXIT;
        loop {
            if let Some(status) = child.try_wait()? {
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
        if let Some(child) = self.child.take() {
            end_or_keep(child, Child::kill);
        }
    }
}

/// Reaps a brain once `kill` ends it. One whose kill failed is kept for the gate instead, since
/// waiting on a brain that wasn't killed would never return.
fn end_or_keep(mut child: Child, kill: impl FnOnce(&mut Child) -> io::Result<()>) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    match kill(&mut child) {
        Ok(()) => {
            let _reaped = child.wait();
        }
        Err(_) => keep(child),
    }
}

fn keep(child: Child) {
    LEFTOVERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(child);
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{end_or_keep, leftovers};

    #[test]
    fn a_brain_whose_kill_fails_is_kept_for_the_gate_not_waited_on() {
        let child = Command::new("cmd")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd starts");
        let pid = child.id();
        let (done, returned) = mpsc::channel();
        thread::spawn(move || {
            end_or_keep(child, |_| Err(io::Error::other("refused")));
            done.send(()).expect("the test waits");
        });
        let returned = returned.recv_timeout(Duration::from_secs(10));
        let mut kept = leftovers();
        let pids: Vec<_> = kept.iter().map(Child::id).collect();
        for brain in &mut kept {
            brain.kill().expect("the test child can be killed");
            brain.wait().expect("the test child is reaped");
        }
        assert!(returned.is_ok(), "dropping it returned");
        assert_eq!(pids, [pid]);
    }
}
