use kumochi_protocol::frame::{self, FrameError, Pipe};
use kumochi_protocol::message::{
    DataToShell, EventId, MAX_PAGE_BYTES, MAX_PAGE_ROWS, PageEvent, PageEventKind, PageOf, RunId,
    ThreadCursor, ThreadId, ThreadRow,
};
use rusqlite::{Row, params};

use super::{Store, StoreError, row_id};

#[cfg(test)]
mod tests;

impl Store {
    /// Reads one page, including its continuation, within the full encoded-frame budget.
    /// A single oversized title or event is shortened at a character boundary to make progress.
    ///
    /// # Errors
    /// Fails with a content-free error when reading or encoding fails.
    pub fn read_page(&self, request: u32, page: PageOf) -> Result<DataToShell, StoreError> {
        let mut cursors = Vec::new();
        let mut reply = match page {
            PageOf::Threads { search, after } => {
                let rows = match search {
                    Some(words) => self.search_rows(&words, after, MAX_PAGE_ROWS)?,
                    None => self.thread_rows(after, MAX_PAGE_ROWS)?,
                };
                let (threads, next);
                (threads, cursors, next) = paged(rows, MAX_PAGE_ROWS);
                DataToShell::ThreadList {
                    request,
                    threads,
                    next,
                }
            }
            PageOf::Events { thread, before } => {
                let (events, before) = self.events_page(thread, before, MAX_PAGE_ROWS)?;
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
                DataToShell::ThreadList { threads, next, .. } if threads.len() > 1 => {
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
    pub fn thread_page(
        &self,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<(Vec<ThreadRow>, Option<ThreadCursor>), StoreError> {
        let limit = limit.clamp(1, MAX_PAGE_ROWS);
        let (rows, _, next) = paged(self.thread_rows(after, limit)?, limit);
        Ok((rows, next))
    }

    /// Up to `limit + 1` live threads after `after`, each with its own continuation key.
    fn thread_rows(
        &self,
        after: Option<ThreadCursor>,
        limit: usize,
    ) -> Result<Vec<(ThreadRow, ThreadCursor)>, StoreError> {
        let mut query = self
            .conn
            .prepare(
                "SELECT id, title, updated_ms FROM threads
             WHERE state = 'live' AND (?1 IS NULL OR (updated_ms, id) < (?1, ?2))
             ORDER BY updated_ms DESC, id DESC LIMIT ?3",
            )
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    after.map(|key| key.updated_ms),
                    after.map(|key| bounded_id(key.id.0)),
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?
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
        rows.collect::<Result<_, _>>().map_err(|_| StoreError::Read)
    }

    /// Phrase matches with titles, best rank first and descending id for equal ranks.
    /// Limits are clamped to `1..=MAX_PAGE_ROWS`.
    ///
    /// # Errors
    /// Fails with a content-free error when SQLite fails.
    pub fn search_page(
        &self,
        words: &str,
        limit: usize,
    ) -> Result<(Vec<ThreadRow>, Option<ThreadCursor>), StoreError> {
        self.search_page_after(words, None, limit)
    }

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
            .conn
            .prepare(
                "WITH matches AS MATERIALIZED (
                 SELECT t.id, t.title, t.updated_ms, t.state = 'live' AS live,
                 min(f.rank) AS score
                 FROM events_fts f JOIN events e ON e.id = f.rowid
                 JOIN threads t ON t.id = e.thread
                 WHERE events_fts MATCH ?1 GROUP BY t.id
             ),
             anchor AS (
                 SELECT coalesce((SELECT score FROM matches WHERE id = ?2), ?3) AS score
             )
             SELECT m.id, m.title, m.updated_ms, m.score FROM matches m, anchor a
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
                    let rank: f64 = row.get(3)?;
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
    pub fn events_page(
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
            .conn
            .prepare(
                "SELECT e.id, e.run, e.kind, coalesce(e.text, '') FROM events e
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
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?
                ],
                |row| {
                    let kind: String = row.get(2)?;
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
                        text: row.get(3)?,
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

fn thread_row(row: &Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: ThreadId(row_id(row.get(0)?)),
        title: row.get(1)?,
        updated_ms: row.get(2)?,
    })
}

/// The first `limit` rows, their keys, and the continuation when more were read.
fn paged(
    mut rows: Vec<(ThreadRow, ThreadCursor)>,
    limit: usize,
) -> (Vec<ThreadRow>, Vec<ThreadCursor>, Option<ThreadCursor>) {
    let more = rows.len() > limit;
    rows.truncate(limit);
    let (rows, cursors): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    let next = if more { cursors.last().copied() } else { None };
    (rows, cursors, next)
}

#[cfg(test)]
impl Store {
    /// The ids of the first page of live threads matching `words`.
    pub(crate) fn search_ids(&self, words: &str) -> Result<Vec<ThreadId>, StoreError> {
        let (rows, _) = self.search_page(words, MAX_PAGE_ROWS)?;
        Ok(rows.iter().map(|row| row.id).collect())
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
        DataToShell::ThreadList { threads, .. } => threads.first_mut().map(|row| &mut row.title),
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
