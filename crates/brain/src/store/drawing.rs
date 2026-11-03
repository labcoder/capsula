//! Typed drawing events, isolated from searchable text and content-bearing errors.
//!
//! The reducer is the one writer and has checked every write, so a write here checks only that
//! its thread, run and scene belong together. Recovery leaves out a scene whose rows don't read,
//! not the whole batch.

use std::sync::Arc;

use kumochi_protocol::message::{
    MAX_SCENE_LOG_ENTRIES, MAX_SCENE_SNAPSHOT_BYTES, MAX_VISIBLE_SCENES, RunId, SceneHeader,
    SceneId, ThreadId,
};
use kumochi_scene::{Begin, SceneEvent, Step, StopCut, WireId, limits};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::{EventKind, Store, StoreError, insert_event, insert_events, now_ms, row_id, sql_id};
use crate::drawing::{LAYOUT_VERSION, Reply, StoredScene, Target, Write};

const MAX_BODY_BYTES: usize = 256 * limits::MAX_RECORD_BYTES;

/// The newest uncleared typed scenes of live threads, newest first. A durable cut to nothing
/// hides its scene even when the clear after it never landed. `octet_length` of the bare column
/// reads only the stored length, so SQLite never loads an oversized body.
const CANDIDATES: &str = "SELECT b.id, b.thread FROM events b
     JOIN threads t ON t.id = b.thread AND t.state = 'live'
     WHERE b.kind = 'scene_begin' AND b.body IS NOT NULL AND NOT EXISTS (
         SELECT 1 FROM events c WHERE c.thread = b.thread AND c.id > b.id
         AND c.kind IN ('scene_cleared', 'scene_cut')
         AND CASE WHEN octet_length(c.body) <= ?1 THEN CASE WHEN json_valid(c.body)
             THEN json_extract(c.body, '$.scene') = b.id
                 AND (c.kind = 'scene_cleared' OR json_extract(c.body, '$.cut.activated') = 0)
         END END)
     ORDER BY b.id DESC LIMIT ?2";

/// A thread's steps and cuts after its oldest candidate, in order: each row's kind, the
/// scene it names if that reads, and its body only when it names a candidate.
const LOG_ROWS: &str = "SELECT id, kind,
         CASE WHEN octet_length(body) <= ?3 THEN CASE WHEN json_valid(body)
             THEN json_extract(body, '$.scene') END END,
         CASE WHEN octet_length(body) <= ?3 THEN CASE WHEN json_valid(body)
             THEN CASE WHEN json_extract(body, '$.scene') IN (SELECT value FROM json_each(?4))
                 THEN CAST(body AS BLOB) END END END
     FROM events WHERE thread = ?1 AND id > ?2 AND kind IN ('scene_step', 'scene_cut')
     ORDER BY id";

/// What recovery read: the scenes to restore, and the scenes it left out for a bad row, whose
/// clears are owed so they don't come back.
#[derive(Debug, Default, PartialEq)]
pub struct DrawingRecovery {
    /// Scenes whose rows all read, oldest first.
    pub scenes: Vec<StoredScene>,
    /// Scenes left out, oldest first, named by id alone.
    pub skipped: Vec<SceneId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderBody {
    begin: Begin,
    layout_version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepBody<T> {
    scene: SceneId,
    revision: u32,
    layout_version: u32,
    step: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CutBody {
    scene: SceneId,
    revision: u32,
    cut: StopCut,
}

/// A candidate scene as recovery reads its rows.
struct Reading {
    header: SceneHeader,
    log: Vec<SceneEvent>,
    bytes: usize,
    revision: u32,
}

impl Store {
    /// Executes one drawing operation atomically, never indexing its content.
    ///
    /// # Errors
    /// Returns a content-free failure for invalid drawing data or database failures.
    pub fn drawing_write(&mut self, write: &Write) -> Result<Reply, StoreError> {
        if !self.serving {
            return Err(StoreError::Closed);
        }
        self.write_drawing(write).map_err(redact)
    }

    fn write_drawing(&mut self, write: &Write) -> Result<Reply, StoreError> {
        if let Write::Lookup { thread, name } = write {
            let thread = sql_id(thread.0, StoreError::NoThread)?;
            live(&self.conn, thread)?;
            return lookup(&self.conn, thread, name).map(Reply::Lookup);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let reply = match write {
            Write::Lookup { .. } => unreachable!("lookup returned before the transaction"),
            Write::Step {
                thread,
                run,
                target,
                revision,
                layout_version,
                step,
            } => write_step(
                &tx,
                (*thread, *run),
                target,
                (*revision, *layout_version),
                step,
                now,
            )?,
            Write::Cut {
                scene,
                revision,
                cut,
            } => write_cut(&tx, *scene, *revision, cut, now)?,
        };
        tx.commit()?;
        Ok(reply)
    }

    /// Reads the newest four uncleared typed scenes of live threads. A scene whose header or
    /// rows don't read is left out and logged by row id, and so is every scene its thread began
    /// before a row that names no scene; the others still recover.
    ///
    /// # Errors
    /// A fixed, content-free error when SQLite fails.
    pub fn drawing_scenes(&self) -> Result<DrawingRecovery, StoreError> {
        if !self.serving {
            return Err(StoreError::Closed);
        }
        recover(&self.conn).map_err(redact)
    }
}

fn write_step(
    conn: &Connection,
    (thread, run): (ThreadId, RunId),
    target: &Target,
    (revision, layout_version): (u32, u32),
    step: &Step,
    now: i64,
) -> Result<Reply, StoreError> {
    let thread = sql_id(thread.0, StoreError::NoThread)?;
    let run = sql_id(run.0, StoreError::NoRun)?;
    insert_events(conn, thread, run, &[], now)?;
    let scene = match target {
        Target::New(header) => {
            if lookup(conn, thread, &header.begin.scene)?.is_some() {
                return Err(StoreError::Drawing);
            }
            let body = encode(header)?;
            insert_event(
                conn,
                thread,
                Some(run),
                EventKind::SceneBegin,
                None,
                Some(&body),
                now,
            )?;
            SceneId(row_id(conn.last_insert_rowid()))
        }
        Target::Existing(scene) => {
            if typed_thread(conn, *scene)? != thread {
                return Err(StoreError::Drawing);
            }
            *scene
        }
    };
    let body = encode(&StepBody {
        scene,
        revision,
        layout_version,
        step,
    })?;
    insert_event(
        conn,
        thread,
        Some(run),
        EventKind::SceneStep,
        None,
        Some(&body),
        now,
    )?;
    Ok(Reply::Step(scene))
}

fn write_cut(
    conn: &Connection,
    scene: SceneId,
    revision: u32,
    cut: &StopCut,
    now: i64,
) -> Result<Reply, StoreError> {
    let thread = typed_thread(conn, scene)?;
    // A retry after a commit that reported failure finds its own cut; any other is refused.
    if let Some(stored) = cut_at(conn, thread, scene, revision)? {
        return if stored == *cut {
            Ok(Reply::Cut)
        } else {
            Err(StoreError::Drawing)
        };
    }
    let body = encode(&CutBody {
        scene,
        revision,
        cut: cut.clone(),
    })?;
    insert_event(
        conn,
        thread,
        None,
        EventKind::SceneCut,
        None,
        Some(&body),
        now,
    )?;
    Ok(Reply::Cut)
}

fn redact(error: StoreError) -> StoreError {
    match error {
        StoreError::Sql(_) => StoreError::Drawing,
        other => other,
    }
}

fn live(conn: &Connection, thread: i64) -> Result<(), StoreError> {
    conn.query_row(
        "SELECT 1 FROM threads WHERE id = ?1 AND state = 'live'",
        [thread],
        |_| Ok(()),
    )
    .optional()?
    .ok_or(StoreError::NoThread)
}

/// The live thread a typed scene began in, read without its header or log.
fn typed_thread(conn: &Connection, scene: SceneId) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT b.thread FROM events b JOIN threads t ON t.id = b.thread AND t.state = 'live'
         WHERE b.id = ?1 AND b.kind = 'scene_begin' AND b.body IS NOT NULL",
        [sql_id(scene.0, StoreError::Drawing)?],
        |row| row.get(0),
    )
    .optional()?
    .ok_or(StoreError::Drawing)
}

/// The cut already stored for `scene` at `revision`, if any.
fn cut_at(
    conn: &Connection,
    thread: i64,
    scene: SceneId,
    revision: u32,
) -> Result<Option<StopCut>, StoreError> {
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT CAST(body AS BLOB) FROM events
             WHERE thread = ?1 AND id > ?2 AND kind = 'scene_cut'
             AND CASE WHEN octet_length(body) <= ?4 THEN CASE WHEN json_valid(body)
                 THEN json_extract(body, '$.scene') = ?2 AND json_extract(body, '$.revision') = ?3
             END END
             ORDER BY id LIMIT 1",
            params![
                thread,
                sql_id(scene.0, StoreError::Drawing)?,
                revision,
                body_cap()?
            ],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| {
            serde_json::from_slice::<CutBody>(&bytes)
                .map(|body| body.cut)
                .map_err(|_| StoreError::Drawing)
        })
        .transpose()
}

fn lookup(conn: &Connection, thread: i64, name: &WireId) -> Result<Option<SceneId>, StoreError> {
    let id = conn
        .query_row(
            "SELECT id FROM events WHERE thread = ?1 AND kind = 'scene_begin'
             AND CASE WHEN octet_length(body) <= ?3 THEN CASE WHEN json_valid(body)
             THEN json_extract(body, '$.begin.scene') END END = ?2 ORDER BY id LIMIT 1",
            params![thread, name.as_str(), body_cap()?],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(id.map(|id| SceneId(row_id(id))))
}

/// Reads the candidates' headers, then each thread's rows once, oldest first. A scene's log
/// stays within the entry and byte bounds, so no more of it is kept than recovery may send.
fn recover(conn: &Connection) -> Result<DrawingRecovery, StoreError> {
    let cap = body_cap()?;
    let limit = i64::try_from(MAX_VISIBLE_SCENES).map_err(|_| StoreError::Drawing)?;
    let mut begins: Vec<(i64, i64)> = conn
        .prepare(CANDIDATES)?
        .query_map(params![cap, limit], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    begins.reverse();
    let mut readings = begins
        .iter()
        .map(|&(id, _)| header(conn, id, cap))
        .collect::<Result<Vec<_>, _>>()?;
    let mut threads: Vec<i64> = begins.iter().map(|&(_, thread)| thread).collect();
    threads.sort_unstable();
    threads.dedup();
    for thread in threads {
        let members: Vec<usize> = (0..begins.len())
            .filter(|&at| begins[at].1 == thread && readings[at].is_some())
            .collect();
        let named: Vec<i64> = members.iter().map(|&at| begins[at].0).collect();
        let Some(&oldest) = named.first() else {
            continue;
        };
        let named = serde_json::to_string(&named).map_err(|_| StoreError::Drawing)?;
        let mut query = conn.prepare(LOG_ROWS)?;
        let mut rows = query.query(params![thread, oldest, cap, named])?;
        while members.iter().any(|&at| readings[at].is_some()) {
            let Some(row) = rows.next()? else {
                break;
            };
            let id: i64 = row.get(0)?;
            let kind = row.get_ref(1)?.as_str().map_err(|_| StoreError::Drawing)?;
            // A row can only belong to a scene that began before it.
            let scene = row
                .get_ref(2)?
                .as_i64()
                .ok()
                .filter(|scene| (1..id).contains(scene));
            let bytes = row.get_ref(3)?.as_blob().ok();
            let mut bad = false;
            for &at in &members {
                let begin = begins[at].0;
                let Some(reading) = readings[at].as_mut() else {
                    continue;
                };
                if begin > id || scene.is_some_and(|scene| scene != begin) {
                    continue;
                }
                // A row that names no scene might be any earlier one's, so each of them goes.
                if scene.is_none() || reading.append(kind, bytes).is_none() {
                    readings[at] = None;
                    bad = true;
                }
            }
            if bad {
                skipped(id);
            }
        }
    }
    let mut recovery = DrawingRecovery::default();
    for ((begin, thread), reading) in begins.into_iter().zip(readings) {
        let scene = SceneId(row_id(begin));
        match reading {
            Some(reading) if reading.revision > 0 => recovery.scenes.push(StoredScene {
                scene,
                thread: ThreadId(row_id(thread)),
                header: reading.header,
                log: reading.log,
            }),
            Some(_) => {
                skipped(begin);
                recovery.skipped.push(scene);
            }
            None => recovery.skipped.push(scene),
        }
    }
    Ok(recovery)
}

/// A candidate's header; `None`, logged, when it's too big, doesn't decode or names another
/// layout.
fn header(conn: &Connection, id: i64, cap: i64) -> Result<Option<Reading>, StoreError> {
    let body: Option<HeaderBody> = conn.query_row(
        "SELECT CASE WHEN octet_length(body) <= ?2 THEN CAST(body AS BLOB) END
         FROM events WHERE id = ?1",
        params![id, cap],
        |row| {
            Ok(row
                .get_ref(0)?
                .as_blob()
                .ok()
                .and_then(|bytes| serde_json::from_slice(bytes).ok()))
        },
    )?;
    let Some(HeaderBody {
        begin,
        layout_version,
    }) = body.filter(|body| body.layout_version == LAYOUT_VERSION)
    else {
        skipped(id);
        return Ok(None);
    };
    Ok(Some(Reading {
        header: SceneHeader {
            begin,
            layout_version,
        },
        log: Vec::new(),
        bytes: 0,
        revision: 0,
    }))
}

impl Reading {
    /// Adds one of the scene's rows; `None` when it doesn't decode, is out of order or passes a
    /// bound. The replay after recovery checks what its steps and cuts mean.
    fn append(&mut self, kind: &str, bytes: Option<&[u8]>) -> Option<()> {
        let bytes = bytes?;
        self.bytes += bytes.len();
        if self.log.len() == MAX_SCENE_LOG_ENTRIES || self.bytes > MAX_SCENE_SNAPSHOT_BYTES {
            return None;
        }
        if kind == "scene_step" {
            let body: StepBody<Step> = serde_json::from_slice(bytes).ok()?;
            if body.revision != self.revision + 1
                || body.layout_version != self.header.layout_version
            {
                return None;
            }
            self.revision = body.revision;
            self.log.push(SceneEvent::Step {
                revision: body.revision,
                step: Arc::new(body.step),
            });
        } else {
            let body: CutBody = serde_json::from_slice(bytes).ok()?;
            if body.revision != self.revision {
                return None;
            }
            self.log.push(SceneEvent::Stop(body.cut));
        }
        Some(())
    }
}

/// Logs a typed row recovery left out, by its id only: its body holds user content.
fn skipped(row: i64) {
    let error = StoreError::SceneRow(row);
    tracing::warn!(%error, "skipped while recovering drawings");
}

fn body_cap() -> Result<i64, StoreError> {
    i64::try_from(MAX_BODY_BYTES).map_err(|_| StoreError::Drawing)
}

fn encode(value: &impl Serialize) -> Result<String, StoreError> {
    let body = serde_json::to_string(value).map_err(|_| StoreError::Drawing)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(StoreError::Drawing);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "drawing_tests.rs"]
mod tests;
