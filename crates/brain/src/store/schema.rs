//! The schema, as forward-only migrations numbered by `PRAGMA user_version`.

use rusqlite::Connection;

use super::StoreError;

/// Each entry moves the schema up one version; append, never edit.
const MIGRATIONS: &[&str] = &[V1, V2];

/// Threads, their append-only events with full-text search, and runs. Foreign keys have no
/// cascade, so deletion names every table and fails loudly if one is missed.
const V1: &str = "
-- AUTOINCREMENT keeps the highest id ever issued, so a deleted thread's id, its runs' ids and
-- its events' ids never come back for new ones that late work could be mistaken for.
CREATE TABLE threads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'live' CHECK (state IN ('live', 'deleting')),
    deletion_generation INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread INTEGER NOT NULL REFERENCES threads (id),
    state TEXT NOT NULL CHECK (state IN ('active', 'done', 'interrupted', 'failed')),
    reason TEXT,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER
) STRICT;
CREATE INDEX runs_thread ON runs (thread);
CREATE UNIQUE INDEX runs_id_thread ON runs (id, thread);
CREATE INDEX runs_active ON runs (state) WHERE state = 'active';

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread INTEGER NOT NULL REFERENCES threads (id),
    run INTEGER,
    kind TEXT NOT NULL,
    text TEXT,
    body TEXT,
    created_ms INTEGER NOT NULL,
    -- An event's run belongs to the same thread, or deleting that run's thread would fail.
    FOREIGN KEY (run, thread) REFERENCES runs (id, thread)
) STRICT;
CREATE INDEX events_thread ON events (thread, id);
CREATE INDEX events_run ON events (run);

CREATE VIRTUAL TABLE events_fts USING fts5 (text, content = 'events', content_rowid = 'id');
-- Deleted rows leave the index at once instead of lingering in old segments until a merge.
INSERT INTO events_fts (events_fts, rank) VALUES ('secure-delete', 1);
-- An update would leave the external-content index holding the old text past a deletion.
-- Only plain INSERT may write events: INSERT OR REPLACE deletes without firing events_fts_delete.
CREATE TRIGGER events_append_only BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TRIGGER events_fts_insert AFTER INSERT ON events WHEN new.text IS NOT NULL BEGIN
    INSERT INTO events_fts (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER events_fts_delete AFTER DELETE ON events WHEN old.text IS NOT NULL BEGIN
    INSERT INTO events_fts (events_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
";

/// The purge a deletion still owes: one row from its second transaction until a clean copy
/// replaces the file.
const V2: &str = "
CREATE TABLE purge_pending (id INTEGER PRIMARY KEY CHECK (id = 1)) STRICT;
";

/// Applies every migration past the file's version, each in its own transaction.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let known = MIGRATIONS.len();
    let applied = usize::try_from(found)
        .ok()
        .filter(|&applied| applied <= known)
        .ok_or(StoreError::NewerSchema { found, known })?;
    for (version, sql) in (1_i64..).zip(MIGRATIONS).skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}
