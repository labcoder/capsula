//! The schema, as forward-only migrations numbered by `PRAGMA user_version`.

use rusqlite::Connection;

use super::StoreError;

/// Each entry moves the schema up one version; append, never edit.
pub(super) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6, V7, V8, V9];

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

/// Each thread's kind, where its title came from, and the model it runs on. `kind` keeps these two
/// values for good: changing a `CHECK` rebuilds the table, so later facts get their own columns.
/// `model` and `effort` are free text, checked against the catalog per run, so a catalog change
/// never needs a migration; a column `CHECK` may name an earlier column, so the pair rule needs no
/// rebuild either.
const V3: &str = "
ALTER TABLE threads ADD COLUMN kind TEXT NOT NULL DEFAULT 'thread'
    CHECK (kind IN ('main', 'thread'));
ALTER TABLE threads ADD COLUMN title_source TEXT NOT NULL DEFAULT 'words'
    CHECK (title_source IN ('words', 'model', 'user'));
ALTER TABLE threads ADD COLUMN model TEXT;
ALTER TABLE threads ADD COLUMN effort TEXT CHECK ((model IS NULL) = (effort IS NULL));
CREATE UNIQUE INDEX threads_main ON threads (kind) WHERE kind = 'main';
";

/// Summaries of a thread's oldest events, which compaction writes and the context builder reads
/// in place of the events they cover. They're derived, so they have no full-text index: search
/// finds the raw events, which stay. A thread's summaries are deleted with it. `model` is NULL
/// when the engine took no model, as the fixture does.
const V4: &str = "
CREATE TABLE summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread INTEGER NOT NULL REFERENCES threads (id),
    first_event INTEGER NOT NULL,
    last_event INTEGER NOT NULL CHECK (last_event >= first_event),
    text TEXT NOT NULL,
    model TEXT,
    created_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX summaries_thread ON summaries (thread, id);
";

/// The tool-call ledger (docs/development/tools.md): one row per committed call, written in the
/// transaction that applies the call's write. Results are tagged JSON, and a history result holds
/// references only, so no task's text is copied here; nothing is indexed. `route` is `model`, or
/// `mcp:` and the launch id in 32 hex digits. A thread's rows are deleted with it.
const V5: &str = "
CREATE TABLE tool_calls (
    run INTEGER NOT NULL,
    thread INTEGER NOT NULL,
    route TEXT NOT NULL CHECK (route = 'model' OR (route GLOB 'mcp:*' AND length(route) = 36)),
    call TEXT NOT NULL CHECK (length(call) BETWEEN 1 AND 64),
    tool TEXT NOT NULL,
    hash BLOB NOT NULL CHECK (length(hash) = 32),
    result TEXT NOT NULL CHECK (length(CAST(result AS BLOB)) <= 65536),
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (run, route, call),
    FOREIGN KEY (run, thread) REFERENCES runs (id, thread)
) STRICT;
CREATE INDEX tool_calls_thread ON tool_calls (thread);
";

/// How far the user heard a spoken run (docs/development/voice.md): the segment, its samples
/// played, and the byte offset in the run's answer text where heard speech ends. All three are
/// NULL, which is unknown and every older run, or all three are set; a column `CHECK` may name
/// earlier columns, so the rule needs no table rebuild.
const V6: &str = "
ALTER TABLE runs ADD COLUMN heard_segment INTEGER CHECK (heard_segment >= 0);
ALTER TABLE runs ADD COLUMN heard_sample INTEGER CHECK (heard_sample >= 0);
ALTER TABLE runs ADD COLUMN heard_text INTEGER CHECK (heard_text >= 0
    AND (heard_segment IS NULL) = (heard_sample IS NULL)
    AND (heard_sample IS NULL) = (heard_text IS NULL));
";

/// Issue actions (docs/development/issues.md): one row per draft with its stage as JSON tagged by
/// `kind`, and the high-water mark of approval revisions, which deletion and purges keep so no
/// revision is issued twice. GitHub's account ids are unsigned 64-bit, so one is canonical
/// decimal text. Draft and Failed never hold an attempt that may have created an issue.
const V7: &str = "
CREATE TABLE actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread INTEGER NOT NULL REFERENCES threads (id),
    version INTEGER NOT NULL CHECK (version >= 1),
    -- A missing or null kind makes IN answer NULL, which a CHECK would let pass.
    stage TEXT NOT NULL CHECK (json_valid(stage) AND coalesce(json_extract(stage, '$.kind') IN
        ('draft', 'preflight', 'posting', 'filed', 'failed', 'unknown', 'discarded'), 0)),
    repo TEXT,
    account_id TEXT CHECK (account_id IS NULL OR (length(account_id) BETWEEN 1 AND 20
        AND account_id GLOB '[1-9]*' AND account_id NOT GLOB '*[^0-9]*')),
    account_login TEXT CHECK ((account_id IS NULL) = (account_login IS NULL)),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    approval INTEGER CHECK (approval >= 1),
    consumed INTEGER CHECK (consumed >= 1),
    uncertain INTEGER NOT NULL DEFAULT 0 CHECK (uncertain BETWEEN 0 AND 4294967295),
    created_ms INTEGER NOT NULL,
    CHECK (json_extract(stage, '$.kind') IN ('draft', 'failed', 'discarded')
        OR (repo IS NOT NULL AND account_id IS NOT NULL)),
    CHECK (uncertain = 0 OR json_extract(stage, '$.kind') NOT IN ('draft', 'failed'))
) STRICT;
CREATE INDEX actions_thread ON actions (thread);

CREATE TABLE approval_mark (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL CHECK (last >= 0)
) STRICT;
INSERT INTO approval_mark (id, last) VALUES (1, 0);
";

/// What a screen hold carried into its run's turn (docs/development/store.md): `attached` for the
/// window's image, `screen` for the screen's picture, or the class saying why it wasn't included.
/// Written with the turn, before any request exists, so it never says the image left the machine.
/// NULL for every other run and every older one. Never image bytes. No `CHECK`, so a later class needs no table rebuild; a value a
/// build doesn't know reads as none.
const V8: &str = "
ALTER TABLE runs ADD COLUMN screen TEXT;
";

/// A thread's saved model choice names its connection. Older choices were all `ChatGPT`'s.
const V9: &str = "
ALTER TABLE threads ADD COLUMN connection TEXT;
UPDATE threads SET connection = 'chatgpt' WHERE model IS NOT NULL;
";

/// Applies every migration past the file's version, each in its own transaction.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrate_to(conn, MIGRATIONS.len())
}

/// Applies the migrations up to `target`, at most the known version; tests build older files.
pub(super) fn migrate_to(conn: &mut Connection, target: usize) -> Result<(), StoreError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let known = MIGRATIONS.len();
    debug_assert!(target <= known, "no migration past the known ones");
    let applied = usize::try_from(found)
        .ok()
        .filter(|&applied| applied <= known)
        .ok_or(StoreError::NewerSchema { found, known })?;
    for (version, sql) in (1_i64..).zip(&MIGRATIONS[..target]).skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CAPS-90 L2: only a test asks for a version past the end, so a mistyped one fails loudly.
    /// The check is a `debug_assert!`, so optimized test builds skip it.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "no migration past the known ones")]
    fn a_target_past_the_known_migrations_is_a_bug() {
        let mut conn = Connection::open_in_memory().expect("opens");
        migrate_to(&mut conn, MIGRATIONS.len() + 1).expect("never returns");
    }
}
