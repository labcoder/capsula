use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    DataToShell, EventId, MAX_PAGE_BYTES, MAX_PAGE_ROWS, PageEvent, PageEventKind, PageOf, RunId,
    ThreadCursor, ThreadId, ThreadKind, ThreadRow,
};
use rusqlite::{Connection, Row, params};

use super::threads::{choice_of, kind_of};
use super::{StoreError, row_id};

#[cfg(test)]
mod tests;

/// The page queries, on the store's own connection or on the page reader's.
pub(crate) struct Pages<'a>(pub(crate) &'a Connection);

#[cfg(test)]
impl super::Store {
    /// The page queries on the store's connection.
    pub(crate) fn pages(&self) -> Pages<'_> {
        Pages(&self.conn)
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
        let mut cursors = Vec::new();
        let mut reply = match page {
            PageOf::Threads { search, after } => {
                let (threads, next);
                (threads, cursors, next) = match search {
                    Some(words) => paged(
                        self.search_rows(&words, after, MAX_PAGE_ROWS)?,
                        MAX_PAGE_ROWS,
                    ),
                    None => self.thread_rows(after, MAX_PAGE_ROWS)?,
                };
                DataToShell::ThreadList {
                    request,
                    threads,
                    next,
                }
            }
            PageOf::Events { thread, before } => {
                let (mut events, mut before) = self.events_page(thread, before, MAX_PAGE_ROWS)?;
                // Each row's own encoded size, so the rows past the budget go in one pass rather
                // than one re-encoding each. Dropping a row moves `before` to a higher id, which
                // never encodes shorter, so this never drops a row that would have fit.
                let full = encoded_len(&DataToShell::Page {
                    request,
                    thread,
                    events: Vec::new(),
                    before,
                })?;
                let sizes = events
                    .iter()
                    .map(encoded_len)
                    .collect::<Result<Vec<_>, _>>()?;
                let mut over = (full + sizes.iter().sum::<usize>()).saturating_sub(MAX_PAGE_BYTES);
                while over > 0 && events.len() > 1 {
                    over = over.saturating_sub(sizes[events.len() - 1]);
                    events.pop();
                    before = events.last().map(|event| event.id);
                }
                DataToShell::Page {
                    request,
                    thread,
                    events,
                    before,
                }
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
                DataToShell::Page { events, before, .. } if events.len() > 1 => {
                    events.pop();
                    *before = events.last().map(|event| event.id);
                }
                _ => {
                    fit_single(&mut reply)?;
                    break;
                }
            }
        }
        Ok(reply)
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
                "SELECT id, title, updated_ms, kind, model, effort FROM threads
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
                 SELECT t.id, t.title, t.updated_ms, t.kind, t.model, t.effort,
                 t.state = 'live' AS live, min(f.rank) AS score
                 FROM events_fts f JOIN events e ON e.id = f.rowid
                 JOIN threads t ON t.id = e.thread
                 WHERE events_fts MATCH ?1 GROUP BY t.id
             ),
             anchor AS (
                 SELECT coalesce((SELECT score FROM matches WHERE id = ?2), ?3) AS score
             )
             SELECT m.id, m.title, m.updated_ms, m.kind, m.model, m.effort, m.score
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
                    let rank: f64 = row.get(6)?;
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
    pub(crate) fn events_page(
        &self,
        thread: ThreadId,
        before: Option<EventId>,
        limit: usize,
    ) -> Result<(Vec<PageEvent>, Option<EventId>), StoreError> {
        let Ok(thread) = i64::try_from(thread.0) else {
            return Ok((Vec::new(), None));
        };
        let limit = limit.clamp(1, MAX_PAGE_ROWS);
        let mut query = self
            .0
            .prepare(
                "SELECT e.id, e.run, e.kind, substr(CAST(e.text AS BLOB), 1, ?4) FROM events e
             JOIN threads t ON t.id = e.thread AND t.state = 'live'
             WHERE e.thread = ?1 AND (?2 IS NULL OR e.id < ?2)
             AND e.kind NOT IN ('scene_begin', 'scene_cleared', 'revealed')
             ORDER BY e.id DESC LIMIT ?3",
            )
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    thread,
                    before.map(|id| bounded_id(id.0)),
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?,
                    i64::try_from(MAX_PAGE_BYTES).map_err(|_| StoreError::Read)?
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
                        "user" => PageEventKind::User,
                        "assistant_text" => PageEventKind::AssistantText,
                        "scene_step" => PageEventKind::SceneStep,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    };
                    Ok(PageEvent {
                        id: EventId(row_id(row.get(0)?)),
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
        let before = if more {
            rows.last().map(|row| row.id)
        } else {
            None
        };
        Ok((rows, before))
    }
}

fn bounded_id(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
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
        )?,
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
