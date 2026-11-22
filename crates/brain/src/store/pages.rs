use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    DataToShell, EventId, MAX_PAGE_BYTES, MAX_PAGE_ROWS, PageEvent, PageEventKind, PageOf, RunId,
    SceneId, TaskFact, TaskRun, TaskRunOutcome, TaskState, ThreadCursor, ThreadId, ThreadKind,
    ThreadRow,
};
use rusqlite::{Connection, Row, params};

use super::drawing::MAX_BODY_BYTES;
use super::threads::{choice_of, kind_of};
use super::{StoreError, now_ms, row_id};

const TASK_QUIET_MS: i64 = 5 * 24 * 60 * 60 * 1_000;

fn issue_draft(body: Option<&[u8]>) -> Result<kumochi_protocol::message::DraftId, rusqlite::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Metadata {
        draft: u64,
    }
    let body = body
        .filter(|body| body.len() <= 32)
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let metadata: Metadata =
        serde_json::from_slice(body).map_err(|_| rusqlite::Error::InvalidQuery)?;
    if metadata.draft == 0 || i64::try_from(metadata.draft).is_err() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(kumochi_protocol::message::DraftId(metadata.draft))
}

#[cfg(test)]
mod tests;

/// The page queries, on the store's own connection or on the page reader's.
pub(crate) struct Pages<'a>(pub(crate) &'a Connection);

#[derive(Clone, Copy)]
enum EventPage {
    Before(Option<EventId>),
    After(EventId),
}

impl EventPage {
    fn bounds(self) -> Option<(i64, i64, &'static str)> {
        match self {
            Self::Before(before) => {
                let upper = before
                    .and_then(|id| i64::try_from(id.0).ok())
                    .map_or(i64::MAX, |id| id - 1);
                Some((0, upper, "DESC"))
            }
            Self::After(after) => {
                let lower = i64::try_from(after.0).ok()?.checked_add(1)?;
                Some((lower, i64::MAX, "ASC"))
            }
        }
    }
}

impl super::context::Snapshot<'_> {
    pub(crate) fn page(&self, request: u32, page: PageOf) -> Result<DataToShell, StoreError> {
        Pages(&self.0).read_page(request, page)
    }
}

#[cfg(test)]
impl super::Store {
    /// The page queries on the store's connection.
    pub(crate) fn pages(&self) -> Pages<'_> {
        Pages(&self.conn)
    }
}

#[cfg(feature = "test-support")]
impl super::Store {
    /// Adds deterministic history to a test thread without changing the machine clock.
    ///
    /// # Errors
    /// Returns an error if the test data cannot be written.
    pub fn append_history_for_tests(
        &mut self,
        thread: ThreadId,
        events: &[(i64, &str)],
    ) -> Result<(), StoreError> {
        let thread = super::sql_id(thread.0, StoreError::NoThread)?;
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO events (thread, kind, created_ms, text)
                 VALUES (?1, 'assistant_text', ?2, ?3)",
            )?;
            for &(created_ms, text) in events {
                insert.execute(params![thread, created_ms, text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Reads a real history response for an offline shell fixture.
    ///
    /// # Errors
    /// Returns a content-free failure when the page cannot be read or encoded.
    pub fn page_for_tests(&self, request: u32, page: PageOf) -> Result<DataToShell, StoreError> {
        Pages(&self.conn).read_page(request, page)
    }
}

impl Pages<'_> {
    /// Reads one page, including its continuation, within the full encoded-frame budget.
    /// A single oversized title or event is shortened at a character boundary to make progress;
    /// on the plain list's first page that's the task after the pinned main thread, so the page
    /// always reaches a task while one exists.
    ///
    /// # Errors
    /// Fails with a content-free error when reading or encoding fails.
    pub(crate) fn read_page(&self, request: u32, page: PageOf) -> Result<DataToShell, StoreError> {
        self.read_page_with_clock(request, page, now_ms)
    }

    fn read_page_with_clock(
        &self,
        request: u32,
        page: PageOf,
        clock: impl FnOnce() -> i64,
    ) -> Result<DataToShell, StoreError> {
        let newer = matches!(page, PageOf::EventsAfter { .. });
        let mut cursors = Vec::new();
        let mut reply = match page {
            PageOf::Threads { search, after } => {
                let (mut threads, next);
                (threads, cursors, next) = match search {
                    Some(words) => paged(
                        self.search_rows(&words, after, MAX_PAGE_ROWS)?,
                        MAX_PAGE_ROWS,
                    ),
                    None => self.thread_rows(after, MAX_PAGE_ROWS)?,
                };
                self.attach_task_facts(&mut threads, clock())?;
                DataToShell::ThreadList {
                    request,
                    threads,
                    next,
                }
            }
            PageOf::Events { thread, before } => {
                self.event_reply(request, thread, EventPage::Before(before))?
            }
            PageOf::EventsAfter { thread, after } => {
                self.event_reply(request, thread, EventPage::After(after))?
            }
        };
        while !fits(&reply)? {
            match &mut reply {
                // A pinned main thread has no cursor, so it's never popped and one task stays.
                DataToShell::ThreadList { threads, next, .. } if cursors.len() > 1 => {
                    threads.pop();
                    cursors.pop();
                    *next = cursors.last().copied();
                }
                DataToShell::Page { events, next, .. } if events.len() > 1 => {
                    events.pop();
                    *next = events.last().map(|event| event.id);
                }
                _ => {
                    fit_single(&mut reply)?;
                    break;
                }
            }
        }
        if newer && let DataToShell::Page { events, .. } = &mut reply {
            events.reverse();
        }
        Ok(reply)
    }

    fn attach_task_facts(&self, threads: &mut [ThreadRow], now: i64) -> Result<(), StoreError> {
        let mut query = self
            .0
            .prepare(
                "SELECT r.id, r.state, r.started_ms,
                        CASE WHEN r.reason IS NOT 'crash' THEN r.ended_ms END
                 FROM threads AS t
                 LEFT JOIN runs AS r ON r.id = (SELECT max(id) FROM runs WHERE thread = t.id)
                 WHERE t.id = ?1",
            )
            .map_err(|_| StoreError::Read)?;
        for thread in threads
            .iter_mut()
            .filter(|row| row.kind == ThreadKind::Thread)
        {
            let fact = query
                .query_row([bounded_id(thread.id.0)], |row| {
                    let latest_run = if let Some(id) = row.get::<_, Option<i64>>(0)? {
                        let outcome = match row.get_ref(1)?.as_str()? {
                            "active" => TaskRunOutcome::Active,
                            "done" => TaskRunOutcome::Done,
                            "interrupted" => TaskRunOutcome::Interrupted,
                            "failed" => TaskRunOutcome::Failed,
                            _ => return Err(rusqlite::Error::InvalidQuery),
                        };
                        Some(TaskRun {
                            id: RunId(row_id(id)),
                            outcome,
                        })
                    } else {
                        None
                    };
                    let started_ms: Option<i64> = row.get(2)?;
                    let ended_ms: Option<i64> = row.get(3)?;
                    let last_activity_ms = started_ms
                        .into_iter()
                        .chain(ended_ms)
                        .fold(thread.updated_ms, i64::max);
                    Ok(TaskFact {
                        latest_run,
                        last_activity_ms,
                        state: task_state(
                            now,
                            last_activity_ms,
                            latest_run.map(|run| run.outcome),
                        ),
                    })
                })
                .map_err(|_| StoreError::Read)?;
            thread.task = Some(fact);
        }
        Ok(())
    }

    fn event_reply(
        &self,
        request: u32,
        thread: ThreadId,
        page: EventPage,
    ) -> Result<DataToShell, StoreError> {
        let (mut events, mut next) = self.event_rows(thread, page, MAX_PAGE_ROWS)?;
        let overhead = encoded_len(&DataToShell::Page {
            request,
            thread,
            events: Vec::new(),
            next: None,
        })? + frame::HEADER_LEN;
        let mut size = overhead;
        let mut take = 0;
        for event in &events {
            size += encoded_len(event)?;
            let more = take + 1 < events.len() || next.is_some();
            let cursor = if more { encoded_len(&event.id)? } else { 0 };
            if size + cursor > MAX_PAGE_BYTES {
                break;
            }
            take += 1;
        }
        let take = take.max(1);
        if take < events.len() {
            events.truncate(take);
            next = events.last().map(|event| event.id);
        }
        Ok(DataToShell::Page {
            request,
            thread,
            events,
            next,
        })
    }

    /// Live threads in descending `(updated_ms, id)` order, with a continuation key.
    /// Limits are clamped to `1..=MAX_PAGE_ROWS`; the statement finishes before returning.
    ///
    /// # Errors
    /// Fails with a content-free error when SQLite fails.
    #[cfg(test)]
    pub(crate) fn thread_page(
        &self,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<(Vec<ThreadRow>, Option<ThreadCursor>), StoreError> {
        let (rows, _, next) = self.thread_rows(after, limit.clamp(1, MAX_PAGE_ROWS))?;
        Ok((rows, next))
    }

    /// Up to `limit` live threads after `after`, the keys of its tasks, and the continuation. The
    /// first page starts with the main thread, which counts toward `limit` but leaves room for a
    /// task; later pages leave it out, so it never repeats and its own updates never move a
    /// cursor. One statement reads them all.
    fn thread_rows(&self, after: Option<ThreadCursor>, limit: usize) -> Result<Listed, StoreError> {
        let mut query = self
            .0
            .prepare(
                "SELECT id, title, updated_ms, kind, model, effort, connection FROM threads
             WHERE state = 'live' AND CASE WHEN ?1 IS NULL THEN 1
                 ELSE kind = 'thread' AND (updated_ms, id) < (?1, ?2) END
             ORDER BY kind = 'main' DESC, updated_ms DESC, id DESC LIMIT ?3",
            )
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    after.map(|key| key.updated_ms),
                    after.map(|key| bounded_id(key.id.0)),
                    // The main thread's place, and one more task to tell whether more follow.
                    i64::try_from(limit + 2).map_err(|_| StoreError::Read)?
                ],
                |row| {
                    let thread = thread_row(row)?;
                    let cursor = ThreadCursor {
                        updated_ms: thread.updated_ms,
                        id: thread.id,
                        rank_bits: 0,
                    };
                    Ok((thread, cursor))
                },
            )
            .map_err(|_| StoreError::Read)?;
        let mut rows: Vec<_> = rows
            .collect::<Result<_, _>>()
            .map_err(|_| StoreError::Read)?;
        let main = rows
            .first()
            .is_some_and(|(row, _)| row.kind == ThreadKind::Main)
            .then(|| rows.remove(0).0);
        let tasks = if main.is_some() {
            limit.saturating_sub(1).max(1)
        } else {
            limit
        };
        let (mut threads, cursors, next) = paged(rows, tasks);
        if let Some(main) = main {
            threads.insert(0, main);
        }
        Ok((threads, cursors, next))
    }

    /// Phrase matches with titles, best rank first and descending id for equal ranks.
    /// Limits are clamped to `1..=MAX_PAGE_ROWS`.
    ///
    /// # Errors
    /// Fails with a content-free error when SQLite fails.
    #[cfg(test)]
    pub(crate) fn search_page(
        &self,
        words: &str,
        limit: usize,
    ) -> Result<(Vec<ThreadRow>, Option<ThreadCursor>), StoreError> {
        self.search_page_after(words, None, limit)
    }

    #[cfg(test)]
    fn search_page_after(
        &self,
        words: &str,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<(Vec<ThreadRow>, Option<ThreadCursor>), StoreError> {
        let limit = limit.clamp(1, MAX_PAGE_ROWS);
        let (rows, _, next) = paged(self.search_rows(words, after, limit)?, limit);
        Ok((rows, next))
    }

    /// Up to `limit + 1` matches after `after`, each with its own continuation key. A
    /// continuation compares against its thread's rank as it is now, since any indexed write
    /// moves every rank; the key carries the rank it had, for when its events are gone. A rank
    /// that isn't finite, read or sent, fails the page.
    fn search_rows(
        &self,
        words: &str,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<Vec<(ThreadRow, ThreadCursor)>, StoreError> {
        let rank = after.map(|key| f64::from_bits(key.rank_bits));
        if rank.is_some_and(|rank| !rank.is_finite()) {
            return Err(StoreError::Read);
        }
        // shortcut: once the cursor's thread is removed its old rank is compared with ranks
        // the removal moved, so rows can be skipped or repeated; snapshot a search's ranks when
        // continuations must survive deletions.
        if words.contains('\0') {
            return Ok(Vec::new());
        }
        // shortcut: every page scores every matching event before taking its rows, ahead of the
        // next write: about 65 ms at 20k matching events and 335 ms at 100k on Rez; OSC-152 moves
        // page reads to their own read-only connection.
        let phrase = format!("\"{}\"", words.replace('"', "\"\""));
        let mut query = self
            .0
            .prepare(
                "WITH matches AS MATERIALIZED (
                 SELECT t.id, t.title, t.updated_ms, t.kind, t.model, t.effort, t.connection,
                 t.state = 'live' AS live, min(f.rank) AS score
                 FROM events_fts f JOIN events e ON e.id = f.rowid
                 JOIN threads t ON t.id = e.thread
                 WHERE events_fts MATCH ?1 GROUP BY t.id
             ),
             anchor AS (
                 SELECT coalesce((SELECT score FROM matches WHERE id = ?2), ?3) AS score
             )
             SELECT m.id, m.title, m.updated_ms, m.kind, m.model, m.effort, m.connection, m.score
             FROM matches m, anchor a
             WHERE m.live AND (?2 IS NULL OR m.score > a.score OR (m.score = a.score AND m.id < ?2))
             ORDER BY m.score, m.id DESC LIMIT ?4",
            )
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    phrase,
                    after.map(|key| bounded_id(key.id.0)),
                    rank,
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?
                ],
                |row| {
                    let thread = thread_row(row)?;
                    let rank: f64 = row.get(7)?;
                    if !rank.is_finite() {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let cursor = ThreadCursor {
                        updated_ms: thread.updated_ms,
                        id: thread.id,
                        rank_bits: rank.to_bits(),
                    };
                    Ok((thread, cursor))
                },
            )
            .map_err(|_| StoreError::Read)?;
        rows.collect::<Result<_, _>>().map_err(|_| StoreError::Read)
    }

    /// A live thread's events, newest first, continuing before the oldest returned id.
    /// Missing and deleting threads return an empty page. Limits are clamped to `1..=MAX_PAGE_ROWS`.
    ///
    /// # Errors
    /// Fails with a content-free error when SQLite fails or a stored event kind is unknown.
    #[cfg(test)]
    pub(crate) fn events_page(
        &self,
        thread: ThreadId,
        before: Option<EventId>,
        limit: usize,
    ) -> Result<(Vec<PageEvent>, Option<EventId>), StoreError> {
        self.event_rows(thread, EventPage::Before(before), limit)
    }

    fn event_rows(
        &self,
        thread: ThreadId,
        page: EventPage,
        limit: usize,
    ) -> Result<(Vec<PageEvent>, Option<EventId>), StoreError> {
        let Ok(thread) = i64::try_from(thread.0) else {
            return Ok((Vec::new(), None));
        };
        let Some((lower, upper, order)) = page.bounds() else {
            return Ok((Vec::new(), None));
        };
        let limit = limit.clamp(1, MAX_PAGE_ROWS);
        let mut query = self
            .0
            .prepare(&format!(
                "SELECT e.id, e.run, e.kind, substr(CAST(e.text AS BLOB), 1, ?4),
                 CASE WHEN e.kind = 'scene_step' THEN
                     CASE WHEN octet_length(e.body) <= ?5 THEN CASE WHEN json_valid(e.body)
                         THEN CASE WHEN json_type(e.body, '$.scene') = 'integer'
                             AND typeof(json_extract(e.body, '$.scene')) = 'integer'
                         THEN (SELECT b.id FROM events b
                             WHERE b.id = json_extract(e.body, '$.scene')
                             AND b.id > 0 AND b.id < e.id AND b.thread = e.thread
                             AND b.kind = 'scene_begin' AND b.body IS NOT NULL)
                     END END END
                 END,
                 CASE WHEN e.kind IN ('draft', 'action') THEN substr(CAST(e.body AS BLOB), 1, 33) END,
                 r.screen, r.state = 'active', r.state = 'failed', e.created_ms
                 FROM events e
             JOIN threads t ON t.id = e.thread AND t.state = 'live'
             LEFT JOIN runs r ON e.kind = 'user' AND r.id = e.run AND r.thread = e.thread
             WHERE e.thread = ?1 AND e.id BETWEEN ?2 AND ?6
             AND e.kind NOT IN ('scene_begin', 'scene_cleared', 'scene_cut', 'revealed',
                                'output_item', 'tool_result')
             ORDER BY e.id {order} LIMIT ?3"
            ))
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    thread,
                    lower,
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?,
                    i64::try_from(MAX_PAGE_BYTES).map_err(|_| StoreError::Read)?,
                    i64::try_from(MAX_BODY_BYTES).map_err(|_| StoreError::Read)?,
                    upper,
                ],
                |row| {
                    let kind: String = row.get(2)?;
                    // No text reaches the page budget, so SQL stops there, in bytes, since its
                    // character functions stop at a NUL; such a row is cut later, or dropped.
                    let mut text = row
                        .get_ref(3)?
                        .as_bytes_or_null()?
                        .unwrap_or_default()
                        .to_vec();
                    if let Err(error) = std::str::from_utf8(&text) {
                        text.truncate(error.valid_up_to());
                    }
                    let kind = match kind.as_str() {
                        "user" => {
                            let active: Option<bool> = row.get(7)?;
                            let failed = row.get::<_, Option<bool>>(8)? == Some(true);
                            let screen = row
                                .get::<_, Option<String>>(6)?
                                .as_deref()
                                .and_then(|screen| super::held::shown(screen, active));
                            match (failed, screen) {
                                (false, None) => PageEventKind::User,
                                (false, Some(screen)) => PageEventKind::HeldTurn { screen },
                                (true, None) => PageEventKind::FailedUser,
                                (true, Some(screen)) => PageEventKind::FailedHeldTurn { screen },
                            }
                        }
                        "assistant_text" => PageEventKind::AssistantText,
                        "scene_step" => row.get::<_, Option<i64>>(4)?.map_or(
                            PageEventKind::SceneStep,
                            |scene| PageEventKind::NoVisual {
                                scene: SceneId(row_id(scene)),
                            },
                        ),
                        "task_deleted" => PageEventKind::TaskDeleted,
                        "draft" | "action" => PageEventKind::Issue {
                            draft: issue_draft(row.get_ref(5)?.as_bytes_or_null()?)?,
                        },
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    };
                    Ok(PageEvent {
                        id: EventId(row_id(row.get(0)?)),
                        created_ms: row.get(9)?,
                        run: row.get::<_, Option<i64>>(1)?.map(|id| RunId(row_id(id))),
                        kind,
                        text: String::from_utf8(text).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        cut: false,
                    })
                },
            )
            .map_err(|_| StoreError::Read)?;
        let mut rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Read)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = rows.last().filter(|_| more).map(|row| row.id);
        Ok((rows, next))
    }
}

fn bounded_id(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

fn task_state(now: i64, activity: i64, outcome: Option<TaskRunOutcome>) -> TaskState {
    if now.saturating_sub(activity) > TASK_QUIET_MS {
        return TaskState::Sleeping;
    }
    match outcome {
        Some(TaskRunOutcome::Failed | TaskRunOutcome::Interrupted) => TaskState::NeedsYou,
        Some(TaskRunOutcome::Done) => TaskState::Done,
        Some(TaskRunOutcome::Active) | None => TaskState::Earlier,
    }
}

/// A row of `id, title, updated_ms, kind, model, effort`.
fn thread_row(row: &Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: ThreadId(row_id(row.get(0)?)),
        title: row.get(1)?,
        updated_ms: row.get(2)?,
        kind: kind_of(row.get_ref(3)?.as_str()?)?,
        choice: choice_of(
            row.get_ref(4)?.as_str_or_null()?,
            row.get_ref(5)?.as_str_or_null()?,
            row.get_ref(6)?.as_str_or_null()?,
        )?,
        task: None,
    })
}

/// A list's rows in order, the keys of the rows a page may end on, and the continuation.
type Listed = (Vec<ThreadRow>, Vec<ThreadCursor>, Option<ThreadCursor>);

/// The first `limit` rows, their keys, and the continuation when more were read.
fn paged(mut rows: Vec<(ThreadRow, ThreadCursor)>, limit: usize) -> Listed {
    let more = rows.len() > limit;
    rows.truncate(limit);
    let (rows, cursors): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    let next = if more { cursors.last().copied() } else { None };
    (rows, cursors, next)
}

#[cfg(test)]
impl Pages<'_> {
    /// The ids of the first page of live threads matching `words`.
    pub(crate) fn search_ids(&self, words: &str) -> Result<Vec<ThreadId>, StoreError> {
        let (rows, _) = self.search_page(words, MAX_PAGE_ROWS)?;
        Ok(rows.iter().map(|row| row.id).collect())
    }
}

/// `value`'s postcard encoding, the frame payload's, in bytes.
fn encoded_len(value: &impl serde::Serialize) -> Result<usize, StoreError> {
    match frame::encode(value, Pipe::Data) {
        Ok(frame) => Ok(frame.len() - frame::HEADER_LEN),
        Err(FrameError::TooLarge { len, .. }) => Ok(len),
        Err(_) => Err(StoreError::Read),
    }
}

fn fits(reply: &DataToShell) -> Result<bool, StoreError> {
    match frame::encode(reply, Pipe::Data) {
        Ok(frame) => Ok(frame.len() <= MAX_PAGE_BYTES),
        Err(FrameError::TooLarge { .. }) => Ok(false),
        Err(_) => Err(StoreError::Read),
    }
}

fn single_text(reply: &mut DataToShell) -> Result<&mut String, StoreError> {
    match reply {
        DataToShell::ThreadList { threads, .. } => threads.last_mut().map(|row| &mut row.title),
        DataToShell::Page { events, .. } => events.first_mut().map(|event| {
            event.cut = true;
            &mut event.text
        }),
        _ => None,
    }
    .ok_or(StoreError::Read)
}

fn fit_single(reply: &mut DataToShell) -> Result<(), StoreError> {
    // shortcut: cut events need within-event paging before their full text can be read.
    let text = std::mem::take(single_text(reply)?);
    let mut low = 0;
    let mut high = text.len().min(MAX_PAGE_BYTES);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let end = text.floor_char_boundary(mid);
        text[..end].clone_into(single_text(reply)?);
        if fits(reply)? {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    text[..text.floor_char_boundary(low)].clone_into(single_text(reply)?);
    if !fits(reply)? {
        return Err(StoreError::Read);
    }
    Ok(())
}
