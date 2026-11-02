//! The purge (architecture v1 §6.4): once a deletion's rows are gone, the store is replaced by a
//! clean copy, so nothing of them survives in a page gap or an FTS index key.
//!
//! The copy is made off the store's thread, from a read-only connection, so the store keeps
//! serving: `VACUUM INTO` takes a snapshot, the FTS index is rebuilt on the snapshot, and a
//! second `VACUUM INTO` writes the result into fresh pages, leaving behind the gaps the rebuild
//! made. The store's thread then swaps the copy in, unless the store was written meanwhile. The
//! swap keeps the old file's data under a second name, so the move doesn't wait for the file
//! system to free it; the purge's files are then removed off the store's thread, and the purge
//! is finished only once none is left.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension as _};

use super::{Store, StoreError, checkpoint_now};
use crate::platform;

/// The snapshot, with the deleted rows' leftovers still in it.
const SNAPSHOT: &str = ".purge-snapshot";
/// The clean copy that replaces the store.
const CLEAN: &str = ".purge-clean";
/// The replaced store, deleted rows' leftovers included, until it's removed. Never opened.
const OLD: &str = ".purge-old";

/// An owed purge, ready to copy the store on another thread.
#[derive(Debug)]
pub struct Owed {
    db: PathBuf,
    /// The store connection's change count before the snapshot.
    writes: u64,
    /// The store's size with the pages still in its WAL; each copy needs about as much.
    bytes: u64,
}

/// A clean copy of the store, ready to swap in.
#[derive(Debug)]
pub struct Copied {
    writes: u64,
}

/// How a swap ended. Whichever way, the purge's files are left for [`Leftovers::remove`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Swap {
    /// The clean copy replaced the store; the old file is left to remove.
    Done,
    /// The store was written after the snapshot, so the copy would lose that write; still owed.
    Stale,
    /// A reader kept the WAL from emptying, so the old file can't be closed cleanly; still owed.
    Blocked,
}

/// Stops a copy, and lets a write wait until the copy has let go of the live store, so no
/// snapshot of it is open while the write commits and the write's pages can be checkpointed.
#[derive(Debug, Default)]
pub struct Cancel {
    stop: AtomicBool,
    /// Held while the copy reads the live store: its drain and its snapshot.
    source: Mutex<()>,
    #[cfg(test)]
    pauses: Pauses,
}

impl Cancel {
    /// Stops the copy, then waits until it holds no connection to the live store. Its drain
    /// can't be stopped once started, so this can wait for it.
    pub fn stop_and_release(&self) {
        self.stop.store(true, Ordering::Relaxed);
        #[cfg(test)]
        self.pauses.waiting();
        drop(self.source.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// A copy that pauses where `pauses` say.
    #[cfg(test)]
    pub(crate) fn pausing(pauses: Pauses) -> Self {
        Self {
            pauses,
            ..Self::default()
        }
    }

    /// Whether the copy was told to stop.
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Where a test holds a copy.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// Before it takes the live store.
    Source,
    /// In the drain, its connection open.
    Drain,
    /// In the snapshot, its read-only connection open.
    Snapshot,
    /// After it let go of the live store.
    Clean,
    /// Before it removes what a stopped or failed copy left.
    Cleanup,
}

/// Holds a copy at `at`, telling `reached` and waiting on `release`.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct Pause {
    pub(crate) at: Point,
    pub(crate) reached: std::sync::mpsc::Sender<()>,
    pub(crate) release: std::sync::mpsc::Receiver<()>,
}

/// A copy's pauses, each taken once, and who hears that a write waits for the copy.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Pauses {
    pub(crate) at: Mutex<Vec<Pause>>,
    pub(crate) waiting: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(test)]
impl Pauses {
    fn reach(&self, at: Point) {
        let pause = {
            let mut pauses = self.at.lock().unwrap_or_else(PoisonError::into_inner);
            let found = pauses.iter().position(|pause| pause.at == at);
            found.map(|found| pauses.remove(found))
        };
        if let Some(pause) = pause {
            pause.reached.send(()).ok();
            pause.release.recv().ok();
        }
    }

    fn waiting(&self) {
        if let Some(waiting) = &self.waiting {
            waiting.send(()).ok();
        }
    }
}

/// The purge's files beside the store: its copies, and the old store a swap leaves.
#[derive(Clone, Debug)]
pub struct Leftovers {
    db: PathBuf,
}

impl Store {
    /// The owed purge, unless nothing is owed or a live thread has an active run: a run's next
    /// write would make the copy stale. A thread whose deletion keeps failing keeps its runs,
    /// but they'll never write again.
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
                "SELECT 1 FROM runs WHERE state = 'active'
                 AND thread IN (SELECT id FROM threads WHERE state = 'live') LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if active {
            return Ok(None);
        }
        let bytes: i64 = self.conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )?;
        Ok(Some(Owed {
            db: self.path.clone(),
            writes: self.conn.total_changes(),
            bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
        }))
    }

    /// The purge's files beside this store.
    #[must_use]
    pub fn leftovers(&self) -> Leftovers {
        Leftovers {
            db: self.path.clone(),
        }
    }

    /// Replaces the store's file with the clean copy: empties the WAL, closes the file, moves the
    /// copy over it and reopens. A write since the snapshot keeps the old file, and so does a
    /// failed move, for example while another process holds the file.
    ///
    /// # Errors
    /// Fails when SQLite fails or the copy can't be moved; the old file stays and the purge stays
    /// owed. Should the reopen fail, whichever file it was, the store is unavailable until
    /// [`Store::reconnect`] succeeds, and operations fail rather than write anywhere else.
    pub fn swap(&mut self, copied: &Copied) -> Result<Swap, StoreError> {
        self.swap_with(copied, |db, old| fs::hard_link(db, old))
    }

    /// [`Store::swap`], keeping the old file's data under `.purge-old` with `link`.
    pub(super) fn swap_with(
        &mut self,
        copied: &Copied,
        link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<Swap, StoreError> {
        if self.conn.total_changes() != copied.writes {
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
        self.serving = false;
        // A WAL left beside the new file would be replayed onto it; one survives the close only
        // while another connection holds the store.
        let moved = no_wal(&self.path).and_then(|()| self.replace(link));
        let reopened = self.reconnect();
        if let (Err(error), Err(_)) = (&moved, &reopened) {
            tracing::warn!(%error, "the move failed before the reopen did");
        }
        reopened?;
        moved.map_err(StoreError::Purge)?;
        Ok(Swap::Done)
    }

    /// Swaps in `copied` but leaves the store unavailable, as a failed reopen does: a folder sits
    /// where the reopened store's WAL goes. Removing the returned folder lets the file open again.
    #[cfg(test)]
    pub(crate) fn swap_failing_reopen(&mut self, copied: &Copied) -> PathBuf {
        let wal = sidecar(&self.path, "-wal");
        let swapped = self.swap_with(copied, |db, old| {
            fs::hard_link(db, old)?;
            fs::create_dir(&wal)
        });
        assert!(swapped.is_err(), "the move worked but the reopen failed");
        wal
    }

    /// Moves the clean copy over the store. The store's name never goes missing: the old file
    /// gets a second name first, so the move only drops a name and doesn't wait while the file
    /// system frees the old file's space.
    fn replace(&self, link: impl FnOnce(&Path, &Path) -> io::Result<()>) -> io::Result<()> {
        let old = sidecar(&self.path, OLD);
        // shortcut: on a volume without hard links the move frees the old file itself, about
        // 0.12 ms per MiB of store on Rez, so the swap passes 100 ms near 850 MiB; free it off
        // the thread there too if large stores must live on such volumes.
        let linked = match link(&self.path, &old) {
            Ok(()) => true,
            Err(error) if platform::links_unsupported(&error) => false,
            Err(error) => return Err(error),
        };
        let moved = platform::replace_file(&sidecar(&self.path, CLEAN), &self.path);
        // Cheap: the store's own name still holds the data.
        if moved.is_err()
            && linked
            && let Err(error) = fs::remove_file(&old)
        {
            tracing::warn!(%error, "the old store's second name stays until the purge removes it");
        }
        moved
    }
}

impl Owed {
    /// The space a copy needs: a snapshot and a clean copy, each about the store's size, which
    /// the old store keeps taking until the purge removes it.
    #[must_use]
    pub fn needs(&self) -> u64 {
        self.bytes.saturating_mul(2)
    }

    /// The folder the copy goes in.
    #[must_use]
    pub fn folder(&self) -> &Path {
        self.db.parent().unwrap_or(&self.db)
    }

    /// Makes the clean copy beside the store. Runs on any thread while the store keeps serving.
    /// `None` when `cancel` stopped it before it finished; nothing is left behind then. It reads
    /// the live store only while holding `cancel`'s source, so a write that stops it waits for
    /// that read alone, never for the rest of the copy or its cleanup.
    ///
    /// # Errors
    /// Fails when SQLite fails or a file can't be written; nothing is left behind.
    pub fn copy(self, cancel: &Arc<Cancel>) -> Result<Option<Copied>, StoreError> {
        let leftovers = Leftovers { db: self.db };
        let made = leftovers
            .remove()
            .map_err(StoreError::Purge)
            .and_then(|()| {
                let db = &leftovers.db;
                #[cfg(test)]
                cancel.pauses.reach(Point::Source);
                let source = cancel.source.lock().unwrap_or_else(PoisonError::into_inner);
                let read = drain(db, cancel)? && snapshot(db, cancel)?;
                drop(source);
                Ok(read && clean(db, cancel)? && compact(db, cancel)?)
            });
        if !matches!(made, Ok(true)) {
            #[cfg(test)]
            cancel.pauses.reach(Point::Cleanup);
            if let Err(error) = leftovers.remove() {
                tracing::warn!(%error, "a purge's copy couldn't be removed; it's retried later");
            }
        }
        Ok(made?.then_some(Copied {
            writes: self.writes,
        }))
    }
}

impl Leftovers {
    /// Whether any is left. One that can't be checked counts as left.
    #[must_use]
    pub fn exist(&self) -> bool {
        files(&self.db).any(|path| !missing(&path))
    }

    /// Removes them all, trying each even after one fails, and succeeds only once none is left.
    /// Runs on any thread; removing the old store takes as long as freeing its space, about
    /// 0.12 ms per MiB on Rez. The old store stays while the store's own file is missing: it may
    /// then be the only copy.
    ///
    /// # Errors
    /// Fails with the first file that couldn't be removed, or when one is still there after its
    /// removal succeeded: another process holding it with delete sharing can keep a deleted
    /// file's name until it closes the file.
    pub fn remove(&self) -> io::Result<()> {
        self.remove_with(|path| fs::remove_file(path))
    }

    /// [`Leftovers::remove`], removing each file with `unlink`.
    pub(super) fn remove_with(&self, unlink: impl Fn(&Path) -> io::Result<()>) -> io::Result<()> {
        let mut first = None;
        for path in files(&self.db) {
            let removed = if path == old(&self.db) && missing(&self.db) {
                Err(io::Error::other(
                    "the store is missing, so its old copy stays",
                ))
            } else {
                unlink(&path)
            };
            match removed {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    first.get_or_insert(error);
                }
                _ => {}
            }
        }
        if first.is_none() && self.exist() {
            first = Some(io::Error::other(
                "a purge file is still there after its removal",
            ));
        }
        first.map_or(Ok(()), Err)
    }
}

/// Every file a purge may leave beside `db`, the old store first.
fn files(db: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    [sidecar(db, OLD)]
        .into_iter()
        .chain([SNAPSHOT, CLEAN].into_iter().flat_map(move |copy| {
            ["", "-journal", "-wal", "-shm"]
                .into_iter()
                .map(move |suffix| sidecar(&sidecar(db, copy), suffix))
        }))
}

/// Whether `path` is known not to exist; one that can't be checked may exist.
pub(super) fn missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// The old store a swap left beside `db`.
pub(super) fn old(db: &Path) -> PathBuf {
    sidecar(db, OLD)
}

/// Copies what the WAL holds into the store's file on its own connection, so the swap's
/// checkpoint on the store's thread finds little left to copy: a large deletion a reader kept in
/// the WAL took the swap past 200 ms at 1 GiB on Rez. Passive, so it never waits on the store's
/// writer or holds it up; it can't be stopped once started, and an op meanwhile shares the disk.
/// False unless it copied the whole WAL: a reader still holding part of it, or another
/// connection checkpointing, would leave that copy to the swap, so the purge waits instead.
pub(super) fn drain(db: &Path, cancel: &Arc<Cancel>) -> Result<bool, StoreError> {
    if cancel.stopped() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    #[cfg(test)]
    cancel.pauses.reach(Point::Drain);
    let (busy, frames, copied): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    // Busy leaves both counts at -1, which match.
    Ok(busy == 0 && copied == frames)
}

/// Takes the snapshot through a read-only connection, which never holds the store's writer.
pub(super) fn snapshot(db: &Path, cancel: &Arc<Cancel>) -> Result<bool, StoreError> {
    // Checked before each step too: a short statement may never call the progress handler.
    if cancel.stopped() {
        return Ok(false);
    }
    let mut reader = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    #[cfg(test)]
    cancel.pauses.reach(Point::Snapshot);
    stoppable(&mut reader, cancel)?;
    finished(reader.execute("VACUUM INTO ?1", [utf8(&sidecar(db, SNAPSHOT))?]))
}

/// Clears the owed row in the snapshot and rebuilds its FTS index from the live events, which
/// drops the deleted rows' index keys.
pub(super) fn clean(db: &Path, cancel: &Arc<Cancel>) -> Result<bool, StoreError> {
    #[cfg(test)]
    cancel.pauses.reach(Point::Clean);
    // Checked before each step too: a short statement may never call the progress handler.
    if cancel.stopped() {
        return Ok(false);
    }
    let mut conn = Connection::open(sidecar(db, SNAPSHOT))?;
    // A scratch file: a crash throws it away, so it needs no journal.
    conn.query_row("PRAGMA journal_mode = OFF", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous = OFF;")?;
    stoppable(&mut conn, cancel)?;
    finished(conn.execute_batch(
        "DELETE FROM purge_pending;
         INSERT INTO events_fts (events_fts) VALUES ('rebuild');
         INSERT INTO events_fts (events_fts) VALUES ('optimize');",
    ))
}

/// Writes the snapshot into fresh pages, so no gap the rebuild left survives, removes the
/// snapshot and syncs the result.
pub(super) fn compact(db: &Path, cancel: &Arc<Cancel>) -> Result<bool, StoreError> {
    // Checked before each step too: a short statement may never call the progress handler.
    if cancel.stopped() {
        return Ok(false);
    }
    let mut conn = Connection::open(sidecar(db, SNAPSHOT))?;
    stoppable(&mut conn, cancel)?;
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

/// Lets `cancel` end a statement on `conn`; every 1,000 VM steps [guess].
pub(super) fn stoppable(conn: &mut Connection, cancel: &Arc<Cancel>) -> rusqlite::Result<()> {
    let cancel = Arc::clone(cancel);
    conn.progress_handler(1000, Some(move || cancel.stopped()))
}

/// False when a stop ended the statement.
pub(super) fn finished<T>(result: rusqlite::Result<T>) -> Result<bool, StoreError> {
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

/// Fails unless the store's WAL is gone.
fn no_wal(db: &Path) -> io::Result<()> {
    match fs::metadata(sidecar(db, "-wal")) {
        Ok(_) => Err(io::Error::other("the WAL outlived the close")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
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
