//! Architecture §6.4 on fixture data: a deleted thread's marker is in no table, no FTS index and
//! no byte on disk once the purge's whole result is in.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use kumochi_protocol::message::{ControlToBrain, ControlToShell, Deleted, Phase, RunId, ThreadId};

use super::brain::Case;
use super::client::{Client, Frame};
use super::{PATIENCE, Row, ended, marker, run_end, store, turn};

/// How long a restarted brain gets to purge before it's asked to quit.
const PURGE_WAIT: Duration = Duration::from_secs(2);
/// Restarts before an unfinished purge fails the item.
const ATTEMPTS: usize = 3;

pub(super) fn run(folder: &Path) -> Result<Row> {
    let gone = marker();
    let kept = marker();
    let mixed = format!("CapsGate{}", &gone["capsgate".len()..]);
    let upper = gone.to_ascii_uppercase();
    let case = Case::new(
        folder,
        "delete",
        &format!("0 text answer {upper} end\n0 step label {gone}\n0 text tail\n"),
    )?;
    let brain = case.start()?;
    let mut client = Client::connect()?;
    let (run, t) = turn(&mut client, None, &format!("Title {mixed} words"))?;
    done(&mut client, run)?;
    client.quit()?;
    brain.exited()?;

    case.script(&format!("0 text keep {kept}\n0 step keep\n"))?;
    let brain = case.start()?;
    let mut client = Client::connect()?;
    let (run, _) = turn(&mut client, None, &format!("Keep {kept}"))?;
    done(&mut client, run)?;
    client.control(&ControlToBrain::DeleteThread {
        thread: t,
        after_turns: 1,
        resend: false,
    })?;
    let outcome = client.until(PATIENCE, |seen| match seen.frame {
        Frame::Control(ControlToShell::ThreadDeleted {
            thread, outcome, ..
        }) if thread == t => Some(outcome),
        _ => None,
    })?;
    brain.kill()?;
    drop(client);
    if outcome == Deleted::NotHidden {
        bail!("the delete hid nothing");
    }

    let mut failures = Vec::new();
    let (attempts, running) = purge(&case, t, &gone, &mut failures)?;
    let (files, hits) = store::files(&case.local, &gone)?;
    let (_, control) = store::files(&case.local, &kept)?;
    let conn = store::open(&case.store())?;
    let rows = store::rows(&conn, &gone)?;
    let kept_rows = store::rows(&conn, &kept)?;
    if !running.1.is_empty() || !hits.is_empty() {
        failures.push(format!(
            "the marker is on disk: {:?} while running, {hits:?} after",
            running.1
        ));
    }
    if !rows.hits.is_empty() || rows.fts > 0 {
        failures.push(format!(
            "the marker is stored: tables {:?}, FTS {}",
            rows.hits, rows.fts
        ));
    }
    if control.is_empty() || kept_rows.hits.is_empty() || kept_rows.fts == 0 {
        failures
            .push("the scan didn't find the kept thread's marker, so it proves nothing".to_owned());
    }
    let detail = format!(
        "marker in T's title, user text, assistant text (upper case) and step label; delete \
         answered {outcome:?}, brain killed and restarted; the purge's whole result (no row, no \
         purge_pending, no purge file) after {attempts} start(s); marker found in 0 of {} files \
         while running and 0 of {files} after quitting (.db, -wal, -shm, logs; UTF-8, any case), \
         0 of {} values in {} tables, 0 FTS matches; the kept thread's marker was found in {} \
         files, {} values and {} FTS matches. Retrieval, summaries and Claude Code (slices 3 and \
         8) aren't covered",
        running.0,
        rows.values,
        rows.tables,
        control.len(),
        kept_rows.hits.len(),
        kept_rows.fts
    );
    Ok(Row::checked(
        "§6.4 deletion on fixture data",
        &failures,
        detail,
    ))
}

/// Starts the brain until its purge's whole result is in: no row of the thread, no
/// `purge_pending` and no purge file. Returns the starts it took, and the files read and the
/// marker's hits while the last brain ran.
fn purge(
    case: &Case,
    t: ThreadId,
    gone: &str,
    failures: &mut Vec<String>,
) -> Result<(usize, (usize, Vec<PathBuf>))> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let brain = case.start()?;
        let client = Client::connect()?;
        thread::sleep(PURGE_WAIT);
        let running = store::live_files(&case.local, gone)?;
        client.quit()?;
        brain.exited()?;
        let purge_left = store::purge_files(&case.data())?;
        let conn = store::open(&case.store())?;
        let pending = store::purge_pending(&conn)?;
        let rows_left = store::thread_rows(&conn, t)?;
        let whole = purge_left.is_empty() && !pending && rows_left == 0;
        if whole || attempts == ATTEMPTS {
            if !whole {
                failures.push(format!(
                    "the purge isn't whole after {attempts} starts: pending {pending}, files                      {purge_left:?}, {rows_left} rows of the thread"
                ));
            }
            return Ok((attempts, running));
        }
    }
}

fn done(client: &mut Client, run: RunId) -> Result<()> {
    let (_, phase) = ended(client, run)?;
    if phase != Phase::Done {
        bail!("run {} ended {phase:?}", run.0);
    }
    run_end(client, run)
}
