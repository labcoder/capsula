//! The brain's SQLite store: threads, their events, runs, full-text search and deletion.
//!
//! One file in WAL mode. Every commit is durable (`synchronous=FULL`), so "saved" means the
//! transaction committed. Deleting a thread is two transactions: the first hides it and bumps its
//! deletion generation, the second removes every row, and a purge then rebuilds the FTS index,
//! vacuums the file and truncates the WAL, so no copy of the deleted text survives in page gaps
//! or FTS index keys (architecture v1 §6.4).

mod schema;
#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capsula_protocol::constants;
use capsula_protocol::message::{RunId, ThreadId};
use rusqlite::{Connection, ErrorCode, InterruptHandle, OptionalExtension as _, params};

/// Why the store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `%LOCALAPPDATA%` isn't set to an absolute path, so the store has no home.
    #[error("%LOCALAPPDATA% is not set to an absolute path")]
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

/// A live thread, as the threads panel lists it. Its `Debug` leaves the title out, so a log
/// line can't carry it.
#[derive(Clone, PartialEq, Eq)]
pub struct Thread {
    /// Its id.
    pub id: ThreadId,
    /// Its title.
    pub title: String,
    /// When it last changed, in Unix milliseconds.
    pub updated_ms: i64,
}

impl std::fmt::Debug for Thread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thread")
            .field("id", &self.id)
            .field("updated_ms", &self.updated_ms)
            .finish_non_exhaustive()
    }
}

/// How far a deletion got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Deletion {
    /// The rows are gone and the purge after them finished.
    Finished,
    /// The rows are gone, but the purge is still owed, so old copies of them may remain in the
    /// files until it's retried.
    Pending,
}

/// How a purge attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Purge {
    /// Nothing is owed: the FTS rebuild, VACUUM and checkpoint all succeeded, or none was due.
    Done,
    /// It gave way to queued store work and is still owed.
    Yielded,
    /// A reader blocked the checkpoint; it's still owed.
    Blocked,
}

/// Counts work queued for the store, so a purge gives way to it: a step checks the count before
/// it starts, and queuing work interrupts a step already running. Clones share the count.
#[derive(Clone)]
pub struct Queue(Arc<QueueShared>);

struct QueueShared {
    state: Mutex<QueueState>,
    interrupt: InterruptHandle,
}

#[derive(Default)]
struct QueueState {
    queued: usize,
    purging: bool,
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").finish_non_exhaustive()
    }
}

impl Queue {
    /// Counts one more piece of work, handed over by `hand_over`, and interrupts a running purge
    /// step. Counting and sending under one lock means a step either sees the count or is
    /// interrupted.
    pub fn push<T>(&self, hand_over: impl FnOnce() -> T) -> T {
        let mut state = self.lock();
        state.queued += 1;
        let handed = hand_over();
        if state.purging {
            self.0.interrupt.interrupt();
        }
        handed
    }

    /// The store took one piece of queued work.
    pub fn pop(&self) {
        let mut state = self.lock();
        state.queued = state.queued.saturating_sub(1);
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // The state is two plain fields, valid whatever a panicking holder left.
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
    /// The purge is still owed after recovery tried it, because a reader blocked its checkpoint.
    pub purge_pending: bool,
    /// Deletions that failed again; their threads stay hidden and the next start retries them.
    pub failed_deletions: usize,
}

/// The open store. Not `Sync`: one thread owns it.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    queue: Queue,
    /// Mirrors the `purge_pending` row.
    purge_pending: bool,
    /// The rebuild and VACUUM succeeded since the last deletion, so only the checkpoint is owed.
    vacuumed: bool,
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
        let purge_pending = conn
            .query_row("SELECT 1 FROM purge_pending", [], |_| Ok(()))
            .optional()?
            .is_some();
        let queue = Queue(Arc::new(QueueShared {
            state: Mutex::default(),
            interrupt: conn.get_interrupt_handle(),
        }));
        Ok(Self {
            conn,
            queue,
            purge_pending,
            vacuumed: false,
        })
    }

    /// The count of work queued for this store, which a purge gives way to.
    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    /// Whether a deletion's purge is still owed.
    pub fn purge_pending(&self) -> bool {
        self.purge_pending
    }

    /// The recovery scan (architecture v1 §2.3): runs left active become `failed(crash)`,
    /// deletions left pending finish, and an owed purge runs. Run it once at start, before any new run.
    ///
    /// shortcut: two sessions of the same user run two brains on one store, and the second's
    /// recovery fails the first's live runs; that needs Windows Server or multi-session Windows,
    /// so a per-session store or a lock waits until Capsula supports them.
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
        // Nothing is queued yet, so the purge can only finish or meet a reader.
        recovered.purge_pending = self.purge()? != Purge::Done;
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

    /// Live threads whose event text matches `words`, as a phrase, best first.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn search(&self, words: &str, limit: usize) -> Result<Vec<ThreadId>, StoreError> {
        // FTS5 reads the phrase as a C string, so a NUL would cut it short and fail to parse.
        if words.contains('\0') {
            return Ok(Vec::new());
        }
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
        let thread = sql_id(thread.0, StoreError::NoThread)?;
        // An explicit transaction, so an error raised at commit reaches the caller; an
        // autocommit statement read through query_row would drop it.
        let tx = self.conn.transaction()?;
        let generation = tx
            .query_row(
                "UPDATE threads SET state = 'deleting',
                 deletion_generation = deletion_generation + 1
                 WHERE id = ?1 AND state = 'live' RETURNING deletion_generation",
                params![thread],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NoThread)?;
        tx.commit()?;
        Ok(Generation(generation))
    }

    /// Deletion's second transaction: removes every row of a thread [`Store::begin_delete`]
    /// fenced and records the purge as owed, then runs the purge. Returns
    /// [`Deletion::Pending`] when the purge gave way to queued work or met a reader.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't being deleted.
    pub fn finish_delete(&mut self, thread: ThreadId) -> Result<Deletion, StoreError> {
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
        self.remove(thread)?;
        Ok(match self.purge()? {
            Purge::Done => Deletion::Finished,
            Purge::Yielded | Purge::Blocked => Deletion::Pending,
        })
    }

    /// Removes what deleted rows leave behind: FTS `rebuild` drops the separator keys in
    /// `events_fts_idx`, `VACUUM` rewrites every page so no gap keeps an old cell, and a TRUNCATE
    /// checkpoint moves the result into the main file and empties the WAL. Only then is the purge
    /// marked done. Each step gives way to queued work, and an interrupted step rolls back, so
    /// the purge stays owed. A retry in the same session skips the steps that already succeeded;
    /// after a restart it runs whole.
    ///
    /// # Errors
    /// Fails when SQLite fails for another reason; the purge stays owed.
    pub fn purge(&mut self) -> Result<Purge, StoreError> {
        if !self.purge_pending {
            return Ok(Purge::Done);
        }
        if !self.vacuumed {
            let rebuild = "INSERT INTO events_fts (events_fts) VALUES ('rebuild')";
            if self.step(|conn| conn.execute_batch(rebuild))?.is_none()
                || self.step(|conn| conn.execute_batch("VACUUM"))?.is_none()
            {
                return Ok(Purge::Yielded);
            }
            // Later writes never bring deleted text back, so a retry only owes the checkpoint.
            self.vacuumed = true;
        }
        match self.step(checkpoint_now)? {
            None => return Ok(Purge::Yielded),
            Some(false) => {
                tracing::warn!("a reader blocked the purge's checkpoint; it's retried later");
                return Ok(Purge::Blocked);
            }
            Some(true) => {}
        }
        self.conn.execute("DELETE FROM purge_pending", [])?;
        self.purge_pending = false;
        // The WAL now holds only the cleared row, so a reader blocking this leaves nothing behind.
        if !checkpoint_now(&self.conn)? {
            tracing::debug!("a reader blocked the checkpoint after the purge");
        }
        Ok(Purge::Done)
    }

    /// Runs one purge step unless work is queued; `None` when it gave way, before or during it.
    fn step<T>(
        &self,
        run: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<Option<T>, StoreError> {
        {
            let mut state = self.queue.lock();
            if state.queued > 0 {
                return Ok(None);
            }
            state.purging = true;
        }
        let result = run(&self.conn);
        self.queue.lock().purging = false;
        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::OperationInterrupted =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes a thread's rows and records the purge as owed, in one transaction. The foreign
    /// keys have no cascade, so a table this misses makes the last delete fail rather than leave
    /// rows behind.
    fn remove(&mut self, thread: i64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM runs WHERE thread = ?1", params![thread])?;
        tx.execute("DELETE FROM threads WHERE id = ?1", params![thread])?;
        tx.execute("INSERT OR IGNORE INTO purge_pending (id) VALUES (1)", [])?;
        tx.commit()?;
        self.purge_pending = true;
        self.vacuumed = false;
        Ok(())
    }
}

/// Copies the WAL into the main file and truncates it to zero bytes; false when a reader holding
/// an older snapshot blocked it. It never waits on that reader, so queued work isn't held up.
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
