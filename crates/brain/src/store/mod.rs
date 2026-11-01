//! The brain's SQLite store: threads, their events, runs, full-text search and deletion.
//!
//! One file in WAL mode. Every commit is durable (`synchronous=FULL`), so "saved" means the
//! transaction committed. Deleting a thread is two transactions: the first hides it and bumps its
//! deletion generation, the second removes every row, and a checkpoint then truncates the WAL so
//! no copy of the content stays in the file (architecture v1 §6.4).

mod schema;
#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use capsula_protocol::constants;
use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::{Connection, OptionalExtension as _, params};

/// Why the store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `%LOCALAPPDATA%` isn't set, so the store has no home.
    #[error("%LOCALAPPDATA% is not set")]
    NoLocalAppData,
    /// The store's folder couldn't be created.
    #[error("can't create the store's folder")]
    Folder(#[source] io::Error),
    /// The file was written by a newer Capsula; an older brain never opens it.
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
    /// SQLite failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
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
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::AssistantText => "assistant_text",
            Self::SceneStep => "scene_step",
            Self::Revealed => "revealed",
        }
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

/// A live thread, as the threads panel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    /// Its id.
    pub id: ThreadId,
    /// Its title.
    pub title: String,
    /// When it last changed, in Unix milliseconds.
    pub updated_ms: i64,
}

/// A thread's deletion generation. Work stamped with an older one is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub i64);

/// What the recovery scan changed at start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Runs still active when the last brain died, now `failed(crash)`.
    pub crashed_runs: usize,
    /// Deletions the last brain didn't finish, now finished.
    pub finished_deletions: usize,
    /// Deletions that failed again; their threads stay hidden and the next start retries them.
    pub failed_deletions: usize,
}

/// The open store. Not `Sync`: one thread owns it.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

/// The store's file: `%LOCALAPPDATA%\\<product>\\<store file>`.
///
/// # Errors
/// Fails when `%LOCALAPPDATA%` isn't set.
pub fn default_path() -> Result<PathBuf, StoreError> {
    let dir = std::env::var_os("LOCALAPPDATA").ok_or(StoreError::NoLocalAppData)?;
    Ok(PathBuf::from(dir)
        .join(constants::DATA_DIR)
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
        let mut conn = Connection::open(path)?;
        // journal_mode answers with a row, so it's read rather than set.
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(StoreError::NoWal(mode));
        }
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA secure_delete = ON;
             PRAGMA temp_store = MEMORY;",
        )?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The recovery scan (architecture v1 §2.3): runs left active become `failed(crash)`, and
    /// deletions left pending finish. Run it once at start, before any new run.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn recover(&mut self) -> Result<Recovered, StoreError> {
        let mut recovered = Recovered::default();
        // Deletions first, so their runs aren't counted as crashed.
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
        recovered.crashed_runs = self.conn.execute(
            "UPDATE runs SET state = 'failed', reason = 'crash', ended_ms = ?1
             WHERE state = 'active'",
            params![now_ms()],
        )?;
        // A checkpoint a reader blocked last time gets another chance here.
        self.checkpoint()?;
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
        let (thread, run) = (sql_id(thread.0)?, sql_id(run.0)?);
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

    /// Ends an active run. A run that already ended keeps its first outcome.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoRun`] when the run isn't active.
    pub fn end_run(&mut self, run: RunId, end: RunEnd) -> Result<(), StoreError> {
        let (state, reason) = end.columns();
        let ended = self.conn.execute(
            "UPDATE runs SET state = ?2, reason = ?3, ended_ms = ?4
             WHERE id = ?1 AND state = 'active'",
            params![sql_id(run.0)?, state, reason, now_ms()],
        )?;
        if ended == 0 {
            return Err(StoreError::NoRun);
        }
        Ok(())
    }

    /// Live threads, most recently changed first. A thread being deleted is already gone here.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn threads(&self) -> Result<Vec<Thread>, StoreError> {
        let mut query = self.conn.prepare(
            "SELECT id, title, updated_ms FROM threads WHERE state = 'live'
             ORDER BY updated_ms DESC, id DESC",
        )?;
        let rows = query.query_map([], |row| {
            Ok(Thread {
                id: ThreadId(row_id(row.get(0)?)),
                title: row.get(1)?,
                updated_ms: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Appends an event to an active run of a live thread. `text` is what search finds;
    /// `body` is the typed record as JSON.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't live, and with
    /// [`StoreError::NoRun`] when the run isn't an active run of that thread.
    pub fn append(
        &mut self,
        thread: ThreadId,
        run: RunId,
        kind: EventKind,
        text: Option<&str>,
        body: Option<&str>,
    ) -> Result<(), StoreError> {
        let (thread, run) = (sql_id(thread.0)?, sql_id(run.0)?);
        let now = now_ms();
        let tx = self.conn.transaction()?;
        if !touch(&tx, thread, now)? {
            return Err(StoreError::NoThread);
        }
        let active: Option<i64> = tx
            .query_row(
                "SELECT id FROM runs WHERE id = ?1 AND thread = ?2 AND state = 'active'",
                params![run, thread],
                |row| row.get(0),
            )
            .optional()?;
        if active.is_none() {
            return Err(StoreError::NoRun);
        }
        insert_event(&tx, thread, Some(run), kind, text, body, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Live threads whose event text matches `words`, as a phrase, best first.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn search(&self, words: &str, limit: usize) -> Result<Vec<ThreadId>, StoreError> {
        // A quoted phrase, so the user's text is never parsed as FTS5 query syntax.
        let phrase = format!("\"{}\"", words.replace('"', "\"\""));
        let mut query = self.conn.prepare(
            "SELECT e.thread FROM events_fts f
             JOIN events e ON e.id = f.rowid
             JOIN threads t ON t.id = e.thread AND t.state = 'live'
             WHERE events_fts MATCH ?1
             GROUP BY e.thread ORDER BY min(f.rank) LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = query.query_map(params![phrase, limit], |row| {
            Ok(ThreadId(row_id(row.get(0)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletion's first transaction: from here the thread is hidden, can't be sent as context,
    /// and takes no new work. Returns its new generation; anything stamped with an older one is
    /// stale. Call [`Store::finish_delete`] once its producers are stopped.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't live.
    pub fn begin_delete(&mut self, thread: ThreadId) -> Result<Generation, StoreError> {
        let generation = self
            .conn
            .query_row(
                "UPDATE threads SET state = 'deleting',
                 deletion_generation = deletion_generation + 1
                 WHERE id = ?1 AND state = 'live' RETURNING deletion_generation",
                params![sql_id(thread.0)?],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NoThread)?;
        Ok(Generation(generation))
    }

    /// Deletion's second transaction: removes every row of a thread [`Store::begin_delete`]
    /// fenced, then truncates the WAL.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't being deleted.
    pub fn finish_delete(&mut self, thread: ThreadId) -> Result<(), StoreError> {
        let thread = sql_id(thread.0)?;
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
        self.remove(thread)?;
        self.checkpoint()
    }

    /// Deletes a thread's rows in one transaction. The foreign keys have no cascade, so a table
    /// this misses makes the last delete fail rather than leave rows behind.
    fn remove(&mut self, thread: i64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM runs WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM threads WHERE id = ?1", params![thread])?;
        tx.commit()?;
        Ok(())
    }

    /// Copies the WAL into the main file and truncates it to zero bytes, so deleted content
    /// survives in neither.
    fn checkpoint(&self) -> Result<(), StoreError> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            tracing::warn!("a reader blocked the checkpoint; the next start retries it");
        }
        Ok(())
    }
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
/// no thread or run.
fn sql_id(id: u64) -> Result<i64, StoreError> {
    i64::try_from(id).map_err(|_| StoreError::NoThread)
}
