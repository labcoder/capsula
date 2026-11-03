//! Typed drawing events, isolated from searchable text and content-bearing errors.

use std::sync::Arc;

use kumochi_protocol::message::{
    MAX_SCENE_LOG_ENTRIES, MAX_SCENE_SNAPSHOT_BYTES, RunId, SceneHeader, SceneId, ThreadId,
};
use kumochi_scene::{Begin, SceneEvent, Step, StopCut, WireId, limits};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{EventKind, Store, StoreError, insert_event, insert_events, now_ms, row_id, sql_id};
use crate::drawing::{Reply, StoredScene, Target, Write};

const MAX_BODY_BYTES: usize = 256 * limits::MAX_RECORD_BYTES;

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

    /// Reads the newest four uncleared typed scenes of live threads, oldest first.
    ///
    /// # Errors
    /// Returns a fixed error for corrupt or excessive typed data, without its source.
    pub fn drawing_scenes(&self) -> Result<Vec<StoredScene>, StoreError> {
        if !self.serving {
            return Err(StoreError::Closed);
        }
        let read = || -> Result<Vec<StoredScene>, StoreError> {
            let typed: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE kind = 'scene_begin' AND body IS NOT NULL LIMIT 1)",
                [],
                |row| row.get(0),
            )?;
            if !typed {
                return Ok(Vec::new());
            }
            let max_body_bytes = i64::try_from(MAX_BODY_BYTES).map_err(|_| StoreError::Drawing)?;
            let mut query = self.conn.prepare(
                "SELECT b.id FROM events b JOIN threads t ON t.id = b.thread AND t.state = 'live'
                 WHERE b.kind = 'scene_begin' AND b.body IS NOT NULL AND NOT EXISTS (
                     SELECT 1 FROM events c WHERE c.thread = b.thread AND c.id > b.id
                     AND c.kind = 'scene_cleared' AND CASE WHEN length(CAST(c.body AS BLOB)) <= ?1
                     THEN CASE WHEN json_valid(c.body) THEN json_extract(c.body, '$.scene') END END = b.id)
                 AND NOT EXISTS (
                     SELECT 1 FROM events c WHERE c.thread = b.thread AND c.id > b.id
                     AND c.kind = 'scene_cut' AND CASE
                         WHEN length(CAST(c.body AS BLOB)) <= ?1 THEN CASE WHEN json_valid(c.body)
                         THEN json_extract(c.body, '$.scene') = b.id
                             AND json_extract(c.body, '$.cut.activated') = 0 END END)
                 ORDER BY b.id DESC LIMIT 4",
            )?;
            let ids = query
                .query_map([max_body_bytes], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids.into_iter()
                .rev()
                .map(|id| load(&self.conn, SceneId(drawing_id(id)?)))
                .collect()
        };
        read().map_err(redact)
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
    if !usize::try_from(revision).is_ok_and(|r| (1..=limits::MAX_STEPS).contains(&r)) {
        return Err(StoreError::Drawing);
    }
    let scene = match target {
        Target::New(header) => {
            check_header(header)?;
            if revision != 1
                || header.layout_version != layout_version
                || lookup(conn, thread, &header.begin.scene)?.is_some()
            {
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
            // shortcut: reload bounded history per append; use indexed metadata if cap latency warrants it.
            let stored = load(conn, *scene)?;
            if stored.thread.0 != row_id(thread)
                || stored.header.layout_version != layout_version
                || next_revision(&stored.log) != Some(revision)
                || stored.log.len() >= MAX_SCENE_LOG_ENTRIES
                || stored
                    .log
                    .iter()
                    .any(|event| matches!(event, SceneEvent::Stop(cut) if cut.activated == 0))
            {
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
    let stored = load(conn, scene)?;
    let current = next_revision(&stored.log)
        .and_then(|r| r.checked_sub(1))
        .ok_or(StoreError::Drawing)?;
    let mut at = 0;
    let previous = stored.log.iter().find_map(|event| match event {
        SceneEvent::Step { revision, .. } => {
            at = *revision;
            None
        }
        SceneEvent::Stop(previous) if at == revision => Some(previous),
        SceneEvent::Stop(_) => None,
    });
    if let Some(previous) = previous {
        if previous != cut {
            return Err(StoreError::Drawing);
        }
        return Ok(Reply::Cut);
    }
    if revision != current || stored.log.len() >= MAX_SCENE_LOG_ENTRIES {
        return Err(StoreError::Drawing);
    }
    check_cut(cut, revision)?;
    let mut log = stored.log;
    log.push(SceneEvent::Stop(cut.clone()));
    kumochi_scene::SceneState::replay(stored.header.begin, &log)
        .map_err(|_| StoreError::Drawing)?;
    let body = encode(&CutBody {
        scene,
        revision,
        cut: cut.clone(),
    })?;
    insert_event(
        conn,
        sql_id(stored.thread.0, StoreError::Drawing)?,
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

fn lookup(conn: &Connection, thread: i64, name: &WireId) -> Result<Option<SceneId>, StoreError> {
    let max_body_bytes = i64::try_from(MAX_BODY_BYTES).map_err(|_| StoreError::Drawing)?;
    let id = conn
        .query_row(
            "SELECT id FROM events WHERE thread = ?1 AND kind = 'scene_begin'
         AND CASE WHEN length(CAST(body AS BLOB)) <= ?3 THEN CASE WHEN json_valid(body)
         THEN json_extract(body, '$.begin.scene') END END = ?2 ORDER BY id LIMIT 1",
            params![thread, name.as_str(), max_body_bytes],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    id.map(|id| drawing_id(id).map(SceneId)).transpose()
}

fn load(conn: &Connection, scene: SceneId) -> Result<StoredScene, StoreError> {
    let id = sql_id(scene.0, StoreError::Drawing)?;
    let max_body_bytes = i64::try_from(MAX_BODY_BYTES).map_err(|_| StoreError::Drawing)?;
    let (thread, bytes): (i64, Option<Vec<u8>>) = conn.query_row(
        "SELECT b.thread, CASE WHEN length(CAST(b.body AS BLOB)) <= ?2
         THEN substr(CAST(b.body AS BLOB), 1, ?2) END FROM events b
         JOIN threads t ON t.id = b.thread AND t.state = 'live'
         WHERE b.id = ?1 AND b.kind = 'scene_begin' AND NOT EXISTS (
             SELECT 1 FROM events c WHERE c.thread = b.thread AND c.id > b.id
             AND c.kind = 'scene_cleared' AND CASE WHEN length(CAST(c.body AS BLOB)) <= ?2
             THEN CASE WHEN json_valid(c.body) THEN json_extract(c.body, '$.scene') END END = b.id)",
        params![id, max_body_bytes],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let body: HeaderBody = decode(bytes)?;
    let thread_id = ThreadId(drawing_id(thread)?);
    let header = SceneHeader {
        begin: body.begin,
        layout_version: body.layout_version,
    };
    check_header(&header)?;
    let mut query = conn.prepare(
        "SELECT kind, CASE WHEN length(CAST(body AS BLOB)) <= ?3
         THEN substr(CAST(body AS BLOB), 1, ?3) END FROM events
         WHERE thread = ?1 AND id > ?2 AND kind IN ('scene_step', 'scene_cut')
         AND CASE WHEN length(CAST(body AS BLOB)) <= ?3 THEN CASE WHEN json_valid(body)
             THEN coalesce(json_extract(body, '$.scene') = ?2, 1) ELSE 1 END ELSE 1 END
         ORDER BY id LIMIT 129",
    )?;
    let mut rows = query.query(params![thread, id, max_body_bytes])?;
    let mut log = Vec::new();
    let mut body_bytes = 0;
    let mut revision = 0;
    let mut cut_revision = None;
    while let Some(row) = rows.next()? {
        if log.len() == MAX_SCENE_LOG_ENTRIES {
            return Err(StoreError::Drawing);
        }
        let kind: String = row.get(0)?;
        let bytes: Option<Vec<u8>> = row.get(1)?;
        body_bytes += bytes.as_ref().map_or(0, Vec::len);
        if body_bytes > MAX_SCENE_SNAPSHOT_BYTES {
            return Err(StoreError::Drawing);
        }
        if kind == "scene_step" {
            let body: StepBody<Step> = decode(bytes)?;
            if body.scene != scene
                || body.revision != revision + 1
                || body.revision
                    > u32::try_from(limits::MAX_STEPS).map_err(|_| StoreError::Drawing)?
                || body.layout_version != header.layout_version
            {
                return Err(StoreError::Drawing);
            }
            revision = body.revision;
            log.push(SceneEvent::Step {
                revision,
                step: Arc::new(body.step),
            });
        } else {
            let body: CutBody = decode(bytes)?;
            if body.scene != scene || body.revision != revision || cut_revision == Some(revision) {
                return Err(StoreError::Drawing);
            }
            check_cut(&body.cut, revision)?;
            cut_revision = Some(revision);
            log.push(SceneEvent::Stop(body.cut));
        }
    }
    if revision == 0 {
        return Err(StoreError::Drawing);
    }
    Ok(StoredScene {
        scene,
        thread: thread_id,
        header,
        log,
    })
}

fn next_revision(log: &[SceneEvent]) -> Option<u32> {
    log.iter().rev().find_map(|event| match event {
        SceneEvent::Step { revision, .. } => revision.checked_add(1),
        SceneEvent::Stop(_) => None,
    })
}

fn drawing_id(raw: i64) -> Result<u64, StoreError> {
    u64::try_from(raw)
        .ok()
        .filter(|id| *id != 0)
        .ok_or(StoreError::Drawing)
}

fn check_header(header: &SceneHeader) -> Result<(), StoreError> {
    if header.layout_version != 1 {
        return Err(StoreError::Drawing);
    }
    Ok(())
}

fn check_cut(cut: &StopCut, revision: u32) -> Result<(), StoreError> {
    if revision == 0
        || cut.activated > revision
        || cut.revealed.iter().enumerate().any(|(at, handle)| {
            handle.revision() > cut.activated || cut.revealed[..at].contains(handle)
        })
    {
        return Err(StoreError::Drawing);
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(bytes: Option<Vec<u8>>) -> Result<T, StoreError> {
    serde_json::from_slice(&bytes.ok_or(StoreError::Drawing)?).map_err(|_| StoreError::Drawing)
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
