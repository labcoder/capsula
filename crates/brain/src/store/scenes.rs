//! Scenes in the store: a `scene_begin` event whose id is the `SceneId`, `scene_step` events
//! whose body names their scene, and a `scene_cleared` event for every way a scene leaves the
//! visible set. The visible set is the newest four scenes of live threads with no
//! `scene_cleared`, so recovery at start reads the same set the brain held in memory.

use kumochi_protocol::message::{MAX_VISIBLE_SCENES, RunId, SceneId, Step, ThreadId};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::Deserialize;

use super::{
    EventKind, NewEvent, Store, StoreError, insert_event, insert_events, now_ms, row_id, sql_id,
};

/// Which scene a committed step draws in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneTarget {
    /// A run's first step begins a scene; `evict` is the oldest visible scene when four are.
    New {
        /// The scene a fifth one evicts.
        evict: Option<SceneId>,
    },
    /// A later step of a scene that has its id.
    Existing(SceneId),
}

/// A visible scene: every committed step counts in `revision`, and `kept` holds the steps that
/// stayed after any Stop. Its `Debug` leaves the labels out, so a log line can't carry them.
#[derive(Clone, PartialEq, Eq)]
pub struct SceneState {
    /// The scene.
    pub scene: SceneId,
    /// Its thread.
    pub thread: ThreadId,
    /// The run that drew it.
    pub run: RunId,
    /// Every committed step; it never goes back.
    pub revision: u32,
    /// The steps that stayed.
    pub kept: Vec<Step>,
    /// Its run was stopped and its selection applied, so another Stop can't trim it again.
    pub stopped: bool,
}

impl std::fmt::Debug for SceneState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneState")
            .field("scene", &self.scene)
            .field("thread", &self.thread)
            .field("run", &self.run)
            .field("revision", &self.revision)
            .field("kept", &self.kept.len())
            .field("stopped", &self.stopped)
            .finish()
    }
}

/// A stored step's body.
#[derive(Deserialize)]
struct StepBody {
    index: u32,
    label: String,
}

/// The body of a `scene_cleared` event.
pub(crate) fn cleared_body(scene: SceneId) -> String {
    serde_json::json!({ "scene": scene.0 }).to_string()
}

impl Store {
    /// Commits one step in one transaction with the text streamed before it. A run's first step
    /// also writes the `scene_begin` whose id becomes the scene's and, when a fifth scene evicts
    /// the oldest, that scene's `scene_cleared`, so a crash leaves either the old visible set or
    /// the new one. No id leaves before the commit, so a rolled-back one is never seen.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] when the thread isn't live, and with
    /// [`StoreError::NoRun`] when the run isn't an active run of that thread.
    pub fn commit_step(
        &mut self,
        (thread, run): (ThreadId, RunId),
        text: Option<&str>,
        target: SceneTarget,
        index: u32,
        label: &str,
    ) -> Result<SceneId, StoreError> {
        let (thread, run) = (
            sql_id(thread.0, StoreError::NoThread)?,
            sql_id(run.0, StoreError::NoRun)?,
        );
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let text: Vec<_> = text
            .map(|text| NewEvent {
                kind: EventKind::AssistantText,
                text: Some(text),
                body: None,
            })
            .into_iter()
            .collect();
        insert_events(&tx, thread, run, &text, now)?;
        let scene = match target {
            SceneTarget::New { evict } => {
                insert_event(
                    &tx,
                    thread,
                    Some(run),
                    EventKind::SceneBegin,
                    None,
                    None,
                    now,
                )?;
                let scene = SceneId(row_id(tx.last_insert_rowid()));
                if let Some(evicted) = evict {
                    clear(&tx, evicted, now)?;
                }
                scene
            }
            SceneTarget::Existing(scene) => scene,
        };
        let body = serde_json::json!({ "scene": scene.0, "index": index, "label": label });
        insert_event(
            &tx,
            thread,
            Some(run),
            EventKind::SceneStep,
            Some(label),
            Some(&body.to_string()),
            now,
        )?;
        tx.commit()?;
        Ok(scene)
    }

    /// Records that `scenes` left the visible set, in one transaction. A scene whose thread is
    /// gone or being deleted is skipped: it isn't visible any more.
    ///
    /// # Errors
    /// Fails when SQLite fails; nothing is recorded then.
    pub fn clear_scenes(&mut self, scenes: &[SceneId]) -> Result<(), StoreError> {
        let now = now_ms();
        // Immediate, so a busy store makes it wait like any write: a deferred one that reads
        // before it writes fails at once instead.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &scene in scenes {
            clear(&tx, scene, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records the steps of `scene`'s run that stayed, for a Stop its run's outcome didn't
    /// record. A scene whose thread isn't live, or whose run already has a selection, is skipped.
    ///
    /// # Errors
    /// Fails when SQLite fails; nothing is recorded then.
    pub fn keep_scene(&mut self, scene: SceneId, revealed: &[u32]) -> Result<(), StoreError> {
        let Ok(id) = i64::try_from(scene.0) else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO events (thread, run, kind, text, body, created_ms)
             SELECT b.thread, b.run, ?2, NULL, ?3, ?4 FROM events b
             JOIN threads t ON t.id = b.thread AND t.state = 'live'
             WHERE b.id = ?1 AND b.kind = 'scene_begin' AND NOT EXISTS (
                 SELECT 1 FROM events r WHERE r.run = b.run AND r.kind = ?2
             )",
            params![
                id,
                EventKind::Revealed.as_str(),
                serde_json::json!(revealed).to_string(),
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// The visible set, oldest first: the newest [`MAX_VISIBLE_SCENES`] scenes of live threads
    /// with no `scene_cleared`, each with its committed steps and the ones its run's `revealed`
    /// kept. A step row without a `"scene"` key, or whose body doesn't decode, is skipped, and so
    /// is a scene left with nothing kept.
    ///
    /// shortcut: a crash between a Stop and its `EndRun` loses the `revealed` selection, so every
    /// committed step comes back; persist the Stop earlier when B2 lets scenes span runs.
    /// shortcut: with few scenes uncleared this scans every `scene_begin`; add an index on kind
    /// if a long history makes the start slow.
    ///
    /// # Errors
    /// Fails with a content-free error when SQLite fails.
    pub fn scenes(&self) -> Result<Vec<SceneState>, StoreError> {
        let limit = i64::try_from(MAX_VISIBLE_SCENES).map_err(|_| StoreError::Read)?;
        let begins: Vec<(i64, i64, i64)> = self
            .conn
            .prepare(
                "SELECT b.id, b.thread, b.run FROM events b
                 JOIN threads t ON t.id = b.thread AND t.state = 'live'
                 WHERE b.kind = 'scene_begin' AND NOT EXISTS (
                     SELECT 1 FROM events c
                     WHERE c.thread = b.thread AND c.id > b.id AND c.kind = 'scene_cleared'
                     AND CASE WHEN json_valid(c.body) THEN json_extract(c.body, '$.scene') END = b.id
                 )
                 ORDER BY b.id DESC LIMIT ?1",
            )
            .and_then(|mut query| {
                query
                    .query_map(params![limit], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect()
            })
            .map_err(|_| StoreError::Read)?;
        let mut scenes = Vec::with_capacity(begins.len());
        for (scene, thread, run) in begins.into_iter().rev() {
            let state = self
                .scene(scene, thread, run)
                .map_err(|_| StoreError::Read)?;
            if !state.kept.is_empty() {
                scenes.push(state);
            }
        }
        Ok(scenes)
    }

    /// One scene's steps, rebuilt the way the coordinator keeps them.
    fn scene(&self, scene: i64, thread: i64, run: i64) -> rusqlite::Result<SceneState> {
        let rows: Vec<(i64, Option<i64>, Option<String>)> = self
            .conn
            .prepare(
                "SELECT id, run, body FROM events
                 WHERE thread = ?1 AND id > ?2 AND kind = 'scene_step'
                 AND CASE WHEN json_valid(body) THEN json_extract(body, '$.scene') END = ?2
                 ORDER BY id",
            )?
            .query_map(params![thread, scene], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut steps = Vec::with_capacity(rows.len());
        let mut runs = Vec::new();
        for (row, run, body) in rows {
            match body.as_deref().map(serde_json::from_str::<StepBody>) {
                Some(Ok(step)) => {
                    if !runs.contains(&run) {
                        runs.push(run);
                    }
                    steps.push((run, step));
                }
                _ => skipped(row),
            }
        }
        let mut revealed = Vec::new();
        for run in runs.into_iter().flatten() {
            if let Some(kept) = self.revealed(run)? {
                revealed.push((run, kept));
            }
        }
        let revision = u32::try_from(steps.len()).unwrap_or(u32::MAX);
        let stopped = revealed.iter().any(|(stopped, _)| *stopped == run);
        let kept = steps
            .into_iter()
            .filter(|(run, step)| {
                revealed
                    .iter()
                    .find(|(stopped, _)| Some(*stopped) == *run)
                    .is_none_or(|(_, kept)| kept.contains(&step.index))
            })
            .map(|(_, step)| Step { label: step.label })
            .collect();
        Ok(SceneState {
            scene: SceneId(row_id(scene)),
            thread: ThreadId(row_id(thread)),
            run: RunId(row_id(run)),
            revision,
            kept,
            stopped,
        })
    }

    /// The steps a stopped run kept, or `None` when it wasn't stopped or its record doesn't
    /// decode.
    fn revealed(&self, run: i64) -> rusqlite::Result<Option<Vec<u32>>> {
        let found: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, body FROM events WHERE run = ?1 AND kind = 'revealed'",
                params![run],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((row, body)) = found else {
            return Ok(None);
        };
        let kept = body.as_deref().map(serde_json::from_str::<Vec<u32>>);
        if let Some(Ok(kept)) = kept {
            return Ok(Some(kept));
        }
        skipped(row);
        Ok(None)
    }
}

/// Logs a stored scene row that didn't decode, by its id only: its body holds user content.
fn skipped(row: i64) {
    let error = StoreError::SceneRow(row);
    tracing::warn!(%error, "skipped while recovering scenes");
}

/// Writes `scene`'s `scene_cleared` in its own thread, if that thread is live.
fn clear(conn: &Connection, scene: SceneId, now: i64) -> Result<(), StoreError> {
    let Ok(id) = i64::try_from(scene.0) else {
        return Ok(());
    };
    conn.execute(
        "INSERT INTO events (thread, run, kind, text, body, created_ms)
         SELECT b.thread, NULL, ?2, NULL, ?3, ?4 FROM events b
         JOIN threads t ON t.id = b.thread AND t.state = 'live'
         WHERE b.id = ?1 AND b.kind = 'scene_begin'",
        params![
            id,
            EventKind::SceneCleared.as_str(),
            cleared_body(scene),
            now
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "scene_tests.rs"]
pub(crate) mod tests;
