//! The purge (architecture v1 §6.4): once a deletion's rows are gone, the store is replaced by a
//! clean copy, so nothing of them survives in a page gap or an FTS index key.
//!
//! The copy is made off the store's thread, from a read-only connection, so the store keeps
//! serving: `VACUUM INTO` takes a snapshot, the FTS index is rebuilt on the snapshot, and a
//! second `VACUUM INTO` writes the result into fresh pages, leaving behind the gaps the rebuild
//! made. The store's thread then swaps the copy in, unless the store was written meanwhile.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension as _};

use super::{Store, StoreError, checkpoint_now, connect, owes};

/// The snapshot, with the deleted rows' leftovers still in it.
const SNAPSHOT: &str = ".purge-snapshot";
/// The clean copy that replaces the store.
const CLEAN: &str = ".purge-clean";

/// An owed purge, ready to copy the store on another thread.
#[derive(Debug)]
pub struct Owed {
    db: PathBuf,
    /// The store connection's change count before the snapshot.
    writes: u64,
}

/// A clean copy of the store, ready to swap in.
#[derive(Debug)]
pub struct Copied {
    db: PathBuf,
    writes: u64,
}

/// How a swap ended. The copy is gone either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Swap {
    /// The clean copy replaced the store, and nothing is owed.
    Done,
    /// The store was written after the snapshot, so the copy would lose that write; still owed.
    Stale,
    /// A reader kept the WAL from emptying, so the old file can't be closed cleanly; still owed.
    Blocked,
}

impl Store {
    /// The owed purge, unless nothing is owed or a run is active: a run's next write would make
    /// the copy stale.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn owed(&self) -> Result<Option<Owed>, StoreError> {
        if !self.purge_pending {
            return Ok(None);
        }
        let active = self
            .conn
            .query_row(
                "SELECT 1 FROM runs WHERE state = 'active' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok((!active).then(|| Owed {
            db: self.path.clone(),
            writes: self.conn.total_changes(),
        }))
    }

    /// Replaces the store's file with the clean copy: empties the WAL, closes the file, moves the
    /// copy over it and reopens. A write since the snapshot keeps the old file, and so does a
    /// failed move, for example while a scanner holds the file.
    ///
    /// # Errors
    /// Fails when SQLite fails or the copy can't be moved; the old file stays and the purge stays
    /// owed. Should reopening fail after the move, every later operation fails rather than
    /// write anywhere else.
    pub fn swap(&mut self, copied: Copied) -> Result<Swap, StoreError> {
        let Copied { db, writes } = copied;
        let swapped = self.swap_in(writes);
        let discarded = discard(&db).map_err(StoreError::Purge);
        let swapped = swapped?;
        discarded?;
        Ok(swapped)
    }

    fn swap_in(&mut self, writes: u64) -> Result<Swap, StoreError> {
        if self.conn.total_changes() != writes {
            return Ok(Swap::Stale);
        }
        if !checkpoint_now(&self.conn)? {
            return Ok(Swap::Blocked);
        }
        // Until the reopen, an in-memory database with no tables: an operation fails there.
        let serving = std::mem::replace(&mut self.conn, Connection::open_in_memory()?);
        if let Err((serving, error)) = serving.close() {
            self.conn = serving;
            return Err(error.into());
        }
        // A WAL left beside the new file would be replayed onto it.
        let moved =
            no_wal(&self.path).and_then(|()| fs::rename(sidecar(&self.path, CLEAN), &self.path));
        self.conn = connect(&self.path)?;
        self.purge_pending = owes(&self.conn)?;
        moved.map_err(StoreError::Purge)?;
        Ok(Swap::Done)
    }
}

impl Owed {
    /// Makes the clean copy beside the store. Runs on any thread while the store keeps serving.
    /// `None` when `stop` was set before it finished; nothing is left behind then.
    ///
    /// # Errors
    /// Fails when SQLite fails or a file can't be written; nothing is left behind.
    pub fn copy(self, stop: &Arc<AtomicBool>) -> Result<Option<Copied>, StoreError> {
        let made = discard(&self.db).map_err(StoreError::Purge).and_then(|()| {
            Ok(snapshot(&self.db, stop)? && clean(&self.db, stop)? && compact(&self.db, stop)?)
        });
        if !matches!(made, Ok(true))
            && let Err(error) = discard(&self.db)
        {
            tracing::warn!(%error, "a purge's copy couldn't be removed; the next start removes it");
        }
        Ok(made?.then_some(Copied {
            db: self.db,
            writes: self.writes,
        }))
    }
}

/// Takes the snapshot through a read-only connection, which never holds the store's writer.
pub(super) fn snapshot(db: &Path, stop: &Arc<AtomicBool>) -> Result<bool, StoreError> {
    if stopped(stop) {
        return Ok(false);
    }
    let mut reader = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    stoppable(&mut reader, stop)?;
    finished(reader.execute("VACUUM INTO ?1", [utf8(&sidecar(db, SNAPSHOT))?]))
}

/// Clears the owed row in the snapshot and rebuilds its FTS index from the live events, which
/// drops the deleted rows' index keys.
pub(super) fn clean(db: &Path, stop: &Arc<AtomicBool>) -> Result<bool, StoreError> {
    if stopped(stop) {
        return Ok(false);
    }
    let mut conn = Connection::open(sidecar(db, SNAPSHOT))?;
    // A scratch file: a crash throws it away, so it needs no journal.
    conn.query_row("PRAGMA journal_mode = OFF", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous = OFF;")?;
    stoppable(&mut conn, stop)?;
    finished(conn.execute_batch(
        "DELETE FROM purge_pending;
         INSERT INTO events_fts (events_fts) VALUES ('rebuild');
         INSERT INTO events_fts (events_fts) VALUES ('optimize');",
    ))
}

/// Writes the snapshot into fresh pages, so no gap the rebuild left survives, syncs the result
/// and removes the snapshot.
pub(super) fn compact(db: &Path, stop: &Arc<AtomicBool>) -> Result<bool, StoreError> {
    if stopped(stop) {
        return Ok(false);
    }
    let mut conn = Connection::open(sidecar(db, SNAPSHOT))?;
    stoppable(&mut conn, stop)?;
    let clean = sidecar(db, CLEAN);
    if !finished(conn.execute("VACUUM INTO ?1", [utf8(&clean)?]))? {
        return Ok(false);
    }
    conn.close().map_err(|(_, error)| error)?;
    fs::remove_file(sidecar(db, SNAPSHOT)).map_err(StoreError::Purge)?;
    // VACUUM INTO doesn't sync its output, and the swap moves it over the store.
    fs::OpenOptions::new()
        .write(true)
        .open(&clean)
        .and_then(|file| file.sync_all())
        .map_err(StoreError::Purge)?;
    Ok(true)
}

/// Removes any copy a purge left beside the store.
pub(super) fn discard(db: &Path) -> io::Result<()> {
    for copy in [SNAPSHOT, CLEAN] {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            match fs::remove_file(sidecar(&sidecar(db, copy), suffix)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Lets `stop` end a statement on `conn`; every 1,000 VM steps [guess].
fn stoppable(conn: &mut Connection, stop: &Arc<AtomicBool>) -> rusqlite::Result<()> {
    let stop = Arc::clone(stop);
    conn.progress_handler(1000, Some(move || stop.load(Ordering::Relaxed)))
}

/// Whether `stop` was set, checked before each step too, since a short statement may never
/// call the progress handler.
fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed)
}

/// False when `stop` ended the statement.
fn finished<T>(result: rusqlite::Result<T>) -> Result<bool, StoreError> {
    match result {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == ErrorCode::OperationInterrupted =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Fails unless the store's WAL is gone or empty.
fn no_wal(db: &Path) -> io::Result<()> {
    match fs::metadata(sidecar(db, "-wal")) {
        Ok(wal) if wal.len() > 0 => Err(io::Error::other("the WAL outlived the close")),
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// `db` with `suffix` appended to its file name.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut path = OsString::from(db);
    path.push(suffix);
    path.into()
}

/// A path as SQL text, for `VACUUM INTO`.
fn utf8(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or_else(|| rusqlite::Error::InvalidPath(path.to_owned()).into())
}
