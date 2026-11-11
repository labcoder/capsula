//! The brain's SQLite store: threads, their events, runs, full-text search and deletion.
//!
//! One file in WAL mode. Every commit is durable (`synchronous=FULL`), so "saved" means the
//! transaction committed. Deleting a thread is two transactions: the first hides it and bumps its
//! deletion generation, the second removes every row and records a purge as owed. The purge
//! replaces the file with a clean copy, so no copy of the deleted text survives in page gaps or
//! FTS index keys (architecture v1 §6.4).

mod actions;
#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod admission_tests;
mod calls;
#[cfg(test)]
mod calls_tests;
mod cards;
pub(crate) mod context;
mod drawing;
mod heard;
#[cfg(test)]
mod heard_tests;
mod pages;
mod purge;
pub(crate) mod recall;
mod rounds;
mod scenes;
mod schema;
mod side;
mod summaries;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod thread_tests;
mod threads;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kumochi_protocol::constants;
use kumochi_protocol::message::{Deleted, RunId, ThreadId};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

pub use drawing::DrawingRecovery;
#[cfg(test)]
pub(crate) use pages::Pages;
pub use purge::{Cancel, Copied, Leftovers, Owed, Swap};
#[cfg(test)]
pub(crate) use purge::{Pause, Pauses, Point};
pub(crate) use scenes::cleared_body;
pub use scenes::{SceneState, SceneTarget};
pub use side::Side;
pub use threads::TitleSource;

/// Why the store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `%LOCALAPPDATA%` isn't set to an absolute path, so the store has no home.
    #[error("%LOCALAPPDATA% is not set to an absolute path")]
    NoLocalAppData,
    /// The store's folder couldn't be created.
    #[error("can't create the store's folder")]
    Folder(#[source] io::Error),
    /// The file was written by a newer Kumochi; an older brain never opens it.
    #[error("the store has schema {found}, newer than this build's {known}")]
    NewerSchema {
        /// The file's `user_version`.
        found: i64,
        /// The newest schema this build knows.
        known: usize,
    },
    /// SQLite kept a rollback journal instead of WAL, which would keep deleted pages around.
    #[error("SQLite refused WAL and is in {0} mode")]
    NoWal(String),
    /// The thread doesn't exist or is being deleted, so it takes no new work.
    #[error("no such live thread")]
    NoThread,
    /// The run isn't an active run of that thread, so its late work is refused.
    #[error("no such active run in the thread")]
    NoRun,
    /// A new thread or run id isn't above the highest ever issued.
    #[error("the id was issued before")]
    StaleId,
    /// The store's file is gone but the old store a purge replaced is still beside it, so
    /// something went wrong mid-swap; nothing is opened or removed until a person looks.
    /// shortcut: the brain only logs this and doesn't start; tell the user what to do with
    /// `kumochi.db.purge-old` once the shell can show brain errors.
    #[error("the store's file is missing, but a purge's old copy of it remains")]
    Retired,
    /// A page couldn't be read or encoded; SQLite details may contain user content.
    #[error("can't read the store page")]
    Read,
    /// A replay read exhausted its budget; no partial context is usable.
    #[error("the history read exhausted its budget")]
    RecallBudget,

    /// A drawing write or recovery failed; no database or drawing content is exposed.
    #[error("can't store or recover the drawing")]
    Drawing,

    /// A tool call's write failed; no database or call content is exposed.
    #[error("can't store the tool call")]
    Call,

    /// An issue action's write, read or recovery failed; no database or issue content is exposed.
    #[error("can't store or recover the issue action")]
    Issue,

    /// A swap's reopen failed, or the store's thread ended, so the file is closed to reads.
    #[error("the store's file isn't open")]
    Closed,
    /// A purge's copy of the store couldn't be written, moved or removed.
    #[error("a purge file couldn't be written, moved or removed")]
    Purge(#[source] io::Error),
    /// SQLite failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// A stored scene row didn't decode; it's skipped. Only its id is named, never its content.
    #[error("stored scene row {0} doesn't decode")]
    SceneRow(i64),
    /// A round's native item or result doesn't decode or is over its bound.
    #[error("a stored round record doesn't decode")]
    RoundRecord,
}

/// What an event records. Media are referenced by asset id once assets arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// The user's turn.
    User,
    /// Text the assistant streamed.
    AssistantText,
    /// One committed drawing step.
    SceneStep,
    /// The steps that stayed on screen when the user stopped the run, as a JSON list.
    Revealed,
    /// A scene began; the event's id is the scene's.
    SceneBegin,
    /// A scene left the visible set; the body names it.
    SceneCleared,
    /// An authoritative cut in a typed scene's lifetime log.
    SceneCut,
    /// A native output item of a model round, kept for replay.
    OutputItem,
    /// The result Kumochi answered a model's call with.
    ToolResult,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::AssistantText => "assistant_text",
            Self::SceneStep => "scene_step",
            Self::Revealed => "revealed",
            Self::SceneBegin => "scene_begin",
            Self::SceneCleared => "scene_cleared",
            Self::SceneCut => "scene_cut",
            Self::OutputItem => "output_item",
            Self::ToolResult => "tool_result",
        }
    }
}

/// An event to append. Its `Debug` shows only the kind, so a log line can't carry content.
#[derive(Clone, Copy)]
pub struct NewEvent<'a> {
    /// What it records.
    pub kind: EventKind,
    /// What search finds.
    pub text: Option<&'a str>,
    /// The typed record as JSON.
    pub body: Option<&'a str>,
}

impl std::fmt::Debug for NewEvent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewEvent")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// The engine finished.
    Done,
    /// Stopped before it finished.
    Interrupted(Interruption),
    /// Ended by an error.
    Failed(Failure),
}

/// Why a run was interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    /// The user pressed Esc or Stop.
    Stop,
    /// A new turn began.
    Replaced,
    /// The shell went away.
    ShellLost,
    /// The shell stopped taking data.
    ViewStalled,
    /// The user deleted the run's thread. A thread whose hide didn't commit keeps this outcome.
    Deleted,
}

/// Why a run failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A run budget was exceeded.
    Budget,
    /// The engine failed.
    Engine,
    /// The store refused a write.
    Store,
}

impl RunEnd {
    fn columns(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Done => ("done", None),
            Self::Interrupted(why) => (
                "interrupted",
                Some(match why {
                    Interruption::Stop => "stop",
                    Interruption::Replaced => "replaced",
                    Interruption::ShellLost => "shell_lost",
                    Interruption::ViewStalled => "view_stalled",
                    Interruption::Deleted => "deleted",
                }),
            ),
            Self::Failed(why) => (
                "failed",
                Some(match why {
                    Failure::Budget => "budget",
                    Failure::Engine => "engine",
                    Failure::Store => "store",
                }),
            ),
        }
    }
}

/// A thread's deletion generation. Work stamped with an older one is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub i64);

/// What [`Store::delete`] came to.
#[derive(Debug)]
pub struct Deletion {
    /// How far it got.
    pub outcome: Deleted,
    /// The thread's newest stored run, read before any row went; `None` on `NotHidden`.
    pub last_run: Option<RunId>,
    /// What stopped it short of `Removed`.
    pub error: Option<StoreError>,
}

impl Deletion {
    fn removed(last_run: Option<RunId>) -> Self {
        Self {
            outcome: Deleted::Removed,
            last_run,
            error: None,
        }
    }
}

/// What the recovery scan changed at start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Runs still active when the last brain died, now `failed(crash)`.
    pub crashed_runs: usize,
    /// Deletions the last brain didn't finish, now finished.
    pub finished_deletions: usize,
    /// A deletion's purge is owed; the store's thread runs it once no run is active.
    pub purge_pending: bool,
    /// Deletions that failed again; their threads stay hidden and the next start retries them.
    pub failed_deletions: usize,
    /// Issue sends cut short before anything was sent, now `Failed`.
    pub failed_sends: usize,
    /// Issue sends cut short after one may have been sent, now `Unknown`.
    pub unknown_sends: usize,
}

/// The open store. Not `Sync`: one thread owns it.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: PathBuf,
    /// Mirrors the `purge_pending` row, read only from an open file.
    purge_pending: bool,
    /// False after a swap's reopen failed, until [`Store::reconnect`] opens the file again.
    serving: bool,
    /// The page reader's and the checkpoint helper's connections.
    side: Arc<Side>,
}

/// The store's file, in the data folder.
///
/// # Errors
/// Fails when `%LOCALAPPDATA%` isn't an absolute path.
pub fn default_path() -> Result<PathBuf, StoreError> {
    Ok(crate::platform::data_dir()
        .ok_or(StoreError::NoLocalAppData)?
        .join(constants::STORE_FILE))
}

impl Store {
    /// Opens or creates the store at `path` and brings its schema up to date.
    ///
    /// # Errors
    /// Fails when the folder or file can't be created, the schema is newer than this build's,
    /// or SQLite fails.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(StoreError::Folder)?;
        }
        // Files a purge left are removed by the store's thread, not here on the way to the first
        // turn. Only a store missing beside its old copy stops the start: creating an empty
        // store there would hide the old one.
        if purge::missing(path) && !purge::missing(&purge::old(path)) {
            return Err(StoreError::Retired);
        }
        let mut conn = connect(path, OpenFlags::SQLITE_OPEN_CREATE)?;
        schema::migrate(&mut conn)?;
        let purge_pending = owes(&conn)?;
        Ok(Self {
            conn,
            path: path.to_owned(),
            purge_pending,
            serving: true,
            side: Arc::new(Side::new(path.to_owned())),
        })
    }

    /// Opens the file again after a swap's reopen failed; does nothing while it's open. It never
    /// creates a file, so a missing store stays unavailable rather than coming back empty.
    ///
    /// # Errors
    /// Fails while the file still can't be opened; the store stays unavailable.
    pub fn reconnect(&mut self) -> Result<(), StoreError> {
        if self.serving {
            return Ok(());
        }
        let side = Arc::clone(&self.side);
        let held = side.hold();
        self.reopen()?;
        held.available();
        Ok(())
    }

    /// Opens the file again, reading its owed purge, while the side connections are held.
    fn reopen(&mut self) -> Result<(), StoreError> {
        let conn = connect(&self.path, OpenFlags::empty())?;
        self.purge_pending = owes(&conn)?;
        self.conn = conn;
        self.serving = true;
        Ok(())
    }

    /// The side connections, for the page reader and the checkpoint helper.
    pub fn side(&self) -> Arc<Side> {
        Arc::clone(&self.side)
    }

    /// Whether a deletion's purge is still owed.
    pub fn purge_pending(&self) -> bool {
        self.purge_pending
    }

    /// The recovery scan (architecture v1 §2.3): runs left active become `failed(crash)`,
    /// deletions left pending finish, and the main thread is created if it's missing, so it exists
    /// before [`Store::high_water`] is read. An owed purge is only reported: the store's thread runs
    /// it once no run is active, so a slow or failing purge can't hold up the start. Run it
    /// once at start, before any new run.
    ///
    /// shortcut: two sessions of the same user run two brains on one store, and the second's
    /// recovery fails the first's live runs; that needs Windows Server or multi-session Windows,
    /// so a per-session store or a lock waits until Kumochi supports them.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn recover(&mut self) -> Result<Recovered, StoreError> {
        let mut recovered = Recovered::default();
        let pending: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM threads WHERE state = 'deleting'")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for thread in pending {
            // One stuck deletion mustn't keep the brain from starting; the thread stays hidden.
            match self.remove(thread) {
                Ok(()) => recovered.finished_deletions += 1,
                Err(error) => {
                    tracing::error!(thread, %error, "a pending deletion failed again");
                    recovered.failed_deletions += 1;
                }
            }
        }
        // Only live threads' runs: a thread whose deletion failed again isn't coming back.
        recovered.crashed_runs = self.conn.execute(
            "UPDATE runs SET state = 'failed', reason = 'crash', ended_ms = ?1
             WHERE state = 'active'
             AND thread IN (SELECT id FROM threads WHERE state = 'live')",
            params![now_ms()],
        )?;
        (recovered.failed_sends, recovered.unknown_sends) = self.recover_sends()?;
        self.create_main()?;
        recovered.purge_pending = self.purge_pending;
        Ok(recovered)
    }

    /// The highest thread and run ids ever issued, deleted ones included. New ids must be above
    /// them.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn high_water(&self) -> Result<(ThreadId, RunId), StoreError> {
        let seq = |table: &str| -> Result<u64, StoreError> {
            let seq: Option<i64> = self
                .conn
                .query_row(
                    "SELECT seq FROM sqlite_sequence WHERE name = ?1",
                    params![table],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(seq.map_or(0, row_id))
        };
        Ok((ThreadId(seq("threads")?), RunId(seq("runs")?)))
    }

    /// Stores a user turn in one transaction: the thread when `title` names a new one, the run,
    /// and the user's text. Ids come from the caller and must be above [`Store::high_water`].
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when an existing thread isn't live, and with
    /// [`StoreError::StaleId`] when an id isn't new.
    pub fn begin_turn(
        &mut self,
        thread: ThreadId,
        title: Option<&str>,
        run: RunId,
        text: &str,
    ) -> Result<(), StoreError> {
        let (thread, run) = (
            sql_id(thread.0, StoreError::NoThread)?,
            sql_id(run.0, StoreError::NoRun)?,
        );
        let now = now_ms();
        let tx = self.conn.transaction()?;
        if let Some(title) = title {
            let inserted = tx.execute(
                "INSERT INTO threads (id, title, created_ms, updated_ms)
                 SELECT ?1, ?2, ?3, ?3
                 WHERE ?1 > coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'threads'), 0)",
                params![thread, title, now],
            )?;
            if inserted == 0 {
                return Err(StoreError::StaleId);
            }
        }
        if !touch(&tx, thread, now)? {
            return Err(StoreError::NoThread);
        }
        let inserted = tx.execute(
            "INSERT INTO runs (id, thread, state, started_ms)
             SELECT ?1, ?2, 'active', ?3
             WHERE ?1 > coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'runs'), 0)",
            params![run, thread, now],
        )?;
        if inserted == 0 {
            return Err(StoreError::StaleId);
        }
        insert_event(
            &tx,
            thread,
            Some(run),
            EventKind::User,
            Some(text),
            None,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Appends a run's last `events` and ends it, in one transaction, so its outcome is never
    /// stored without them. A run that already ended keeps its first outcome.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoRun`] when the run isn't an active run of the thread, and
    /// with [`StoreError::NoThread`] when there are events and the thread isn't live.
    pub fn end_run(
        &mut self,
        thread: ThreadId,
        run: RunId,
        events: &[NewEvent<'_>],
        end: RunEnd,
    ) -> Result<(), StoreError> {
        let (thread, run) = (
            sql_id(thread.0, StoreError::NoThread)?,
            sql_id(run.0, StoreError::NoRun)?,
        );
        let tx = self.conn.transaction()?;
        let now = now_ms();
        if !events.is_empty() {
            insert_events(&tx, thread, run, events, now)?;
        }
        let (state, reason) = end.columns();
        let ended = tx.execute(
            "UPDATE runs SET state = ?3, reason = ?4, ended_ms = ?5
             WHERE id = ?1 AND thread = ?2 AND state = 'active'",
            params![run, thread, state, reason, now],
        )?;
        if ended == 0 {
            return Err(StoreError::NoRun);
        }
        tx.commit()?;
        Ok(())
    }

    /// Appends events to an active run of a live thread, in one transaction.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't live, and with
    /// [`StoreError::NoRun`] when the run isn't an active run of that thread.
    pub fn append(
        &mut self,
        thread: ThreadId,
        run: RunId,
        events: &[NewEvent<'_>],
    ) -> Result<(), StoreError> {
        let (thread, run) = (
            sql_id(thread.0, StoreError::NoThread)?,
            sql_id(run.0, StoreError::NoRun)?,
        );
        let tx = self.conn.transaction()?;
        insert_events(&tx, thread, run, events, now_ms())?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a thread whatever state it's in: a live one is hidden, then removed; one already
    /// being deleted is removed; an absent one is already gone. The thread's newest run is read
    /// before any row goes. A failure before the hide commits leaves it `NotHidden`, and after
    /// it `Hidden`, which the next start finishes.
    pub fn delete(&mut self, thread: ThreadId) -> Deletion {
        // An id past i64::MAX names nothing.
        let Ok(id) = i64::try_from(thread.0) else {
            return Deletion::removed(None);
        };
        let found = self
            .conn
            .query_row(
                "SELECT state = 'live', (SELECT max(id) FROM runs WHERE thread = ?1)
                 FROM threads WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional();
        let (live, last_run) = match found {
            Ok(Some((live, run))) => (live, run.map(|run| RunId(row_id(run)))),
            Ok(None) => return Deletion::removed(None),
            Err(error) => {
                return Deletion {
                    outcome: Deleted::NotHidden,
                    last_run: None,
                    error: Some(error.into()),
                };
            }
        };
        if live && let Err(error) = self.begin_delete(thread) {
            return Deletion {
                outcome: Deleted::NotHidden,
                last_run: None,
                error: Some(error),
            };
        }
        match self.finish_delete(thread) {
            Ok(()) => Deletion::removed(last_run),
            Err(error) => Deletion {
                outcome: Deleted::Hidden,
                last_run,
                error: Some(error),
            },
        }
    }

    /// Deletion's first transaction: from here the thread is hidden, can't be sent as context,
    /// and takes no new work, and a task leaves an untitled note in Kumo's conversation, so its
    /// title still leaves the files with it. Returns its new generation; anything stamped with an
    /// older one is stale. Call [`Store::finish_delete`] once its producers are stopped.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't live.
    pub fn begin_delete(&mut self, thread: ThreadId) -> Result<Generation, StoreError> {
        let thread = sql_id(thread.0, StoreError::NoThread)?;
        // An explicit transaction, so an error raised at commit reaches the caller; an
        // autocommit statement read through query_row would drop it.
        let tx = self.conn.transaction()?;
        let (generation, kind): (i64, String) = tx
            .query_row(
                "UPDATE threads SET state = 'deleting',
                 deletion_generation = deletion_generation + 1
                 WHERE id = ?1 AND state = 'live' RETURNING deletion_generation, kind",
                params![thread],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NoThread)?;
        if kind == "thread" {
            tx.execute(
                "INSERT INTO events (thread, run, kind, created_ms)
                 SELECT id, NULL, 'task_deleted', ?1 FROM threads WHERE kind = 'main'",
                params![now_ms()],
            )?;
        }
        self.side.delete_cards(&tx, thread)?;
        tx.commit()?;
        Ok(Generation(generation))
    }

    /// Deletion's second transaction: removes every row of a thread [`Store::begin_delete`]
    /// fenced and records the purge as owed. The store's thread runs the purge once no run is
    /// active; until then old copies of the rows may remain in the files.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't being deleted.
    pub fn finish_delete(&mut self, thread: ThreadId) -> Result<(), StoreError> {
        let thread = sql_id(thread.0, StoreError::NoThread)?;
        let deleting: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM threads WHERE id = ?1 AND state = 'deleting'",
                params![thread],
                |row| row.get(0),
            )
            .optional()?;
        if deleting.is_none() {
            return Err(StoreError::NoThread);
        }
        self.remove(thread)
    }

    /// Deletes a thread's rows and records the purge as owed, in one transaction. The foreign
    /// keys have no cascade, so a table this misses makes the last delete fail rather than leave
    /// rows behind.
    fn remove(&mut self, thread: i64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM tool_calls WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM runs WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM summaries WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM actions WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM threads WHERE id = ?1", params![thread])?;
        tx.execute("INSERT OR IGNORE INTO purge_pending (id) VALUES (1)", [])?;
        tx.commit()?;
        self.purge_pending = true;
        Ok(())
    }
}

/// Opens the store's file in WAL mode with its settings; `create` is
/// `OpenFlags::SQLITE_OPEN_CREATE` where a missing file may be created. Commits never checkpoint:
/// the store's thread hands that to its helper.
fn connect(path: &Path, create: OpenFlags) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | create,
    )?;
    // journal_mode answers with a row, so it's read rather than set.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if mode != "wal" {
        return Err(StoreError::NoWal(mode));
    }
    conn.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA wal_autocheckpoint = 0;
         PRAGMA foreign_keys = ON;
         PRAGMA secure_delete = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

/// Whether the file records an owed purge.
fn owes(conn: &Connection) -> Result<bool, StoreError> {
    Ok(conn
        .query_row("SELECT 1 FROM purge_pending", [], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Copies the WAL into the main file and truncates it to zero bytes; false when a reader holding
/// an older snapshot blocked it. It never waits on that reader.
fn checkpoint_now(conn: &Connection) -> rusqlite::Result<bool> {
    let wait: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    conn.busy_timeout(Duration::ZERO)?;
    let busy = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    });
    conn.busy_timeout(Duration::from_millis(u64::try_from(wait).unwrap_or(0)))?;
    Ok(busy? == 0)
}

/// Inserts events for an active run of a live thread, marking the thread changed.
fn insert_events(
    conn: &Connection,
    thread: i64,
    run: i64,
    events: &[NewEvent<'_>],
    now: i64,
) -> Result<(), StoreError> {
    if !touch(conn, thread, now)? {
        return Err(StoreError::NoThread);
    }
    let active: Option<i64> = conn
        .query_row(
            "SELECT id FROM runs WHERE id = ?1 AND thread = ?2 AND state = 'active'",
            params![run, thread],
            |row| row.get(0),
        )
        .optional()?;
    if active.is_none() {
        return Err(StoreError::NoRun);
    }
    for event in events {
        insert_event(
            conn,
            thread,
            Some(run),
            event.kind,
            event.text,
            event.body,
            now,
        )?;
    }
    Ok(())
}

/// Marks a live thread changed; false when it isn't live.
fn touch(conn: &Connection, thread: i64, now: i64) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "UPDATE threads SET updated_ms = ?2 WHERE id = ?1 AND state = 'live'",
        params![thread, now],
    )? == 1)
}

fn insert_event(
    conn: &Connection,
    thread: i64,
    run: Option<i64>,
    kind: EventKind,
    text: Option<&str>,
    body: Option<&str>,
    now: i64,
) -> Result<(), StoreError> {
    // Only a plain INSERT may write events: OR REPLACE would skip the FTS delete trigger.
    conn.execute(
        "INSERT INTO events (thread, run, kind, text, body, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![thread, run, kind.as_str(), text, body, now],
    )?;
    Ok(())
}

#[cfg(test)]
impl Store {
    /// The raw connection, for tests that inspect rows the API doesn't return.
    pub(crate) fn conn_for_tests(&self) -> &Connection {
        &self.conn
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A rowid as a protocol id.
fn row_id(raw: i64) -> u64 {
    u64::try_from(raw).expect("rowids the store assigns are positive")
}

/// A protocol id as a rowid. An id past `i64::MAX` can only come from a bad peer and names
/// nothing, so it fails with `missing`.
fn sql_id(id: u64, missing: StoreError) -> Result<i64, StoreError> {
    i64::try_from(id).map_err(|_| missing)
}
