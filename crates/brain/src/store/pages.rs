use capsula_protocol::frame::{self, FrameError, Pipe};
use capsula_protocol::message::{
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
        let mut reply = match page {
            PageOf::Threads { search, after } => {
                let (threads, next) = match search {
                    Some(words) => self.search_page_after(&words, after, MAX_PAGE_ROWS)?,
                    None => self.thread_page(after, MAX_PAGE_ROWS)?,
                };
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
                    *next = threads.last().map(cursor);
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
                thread_row,
            )
            .map_err(|_| StoreError::Read)?;
        let rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Read)?;
        Ok(thread_result(rows, limit))
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
        // shortcut: search ranks are live; snapshot them when writes must preserve result order.
        if words.contains('\0') {
            return Ok((Vec::new(), None));
        }
        let phrase = format!("\"{}\"", words.replace('"', "\"\""));
        let limit = limit.clamp(1, MAX_PAGE_ROWS);
        let mut query = self
            .conn
            .prepare(
                "WITH matches AS MATERIALIZED (
                 SELECT t.id, t.title, t.updated_ms, min(f.rank) AS score
                 FROM events_fts f JOIN events e ON e.id = f.rowid
                 JOIN threads t ON t.id = e.thread AND t.state = 'live'
                 WHERE events_fts MATCH ?1 GROUP BY t.id
             )
             SELECT id, title, updated_ms FROM matches
             WHERE ?2 IS NULL OR score > (SELECT score FROM matches WHERE id = ?2)
                OR (score = (SELECT score FROM matches WHERE id = ?2) AND id < ?2)
             ORDER BY score, id DESC LIMIT ?3",
            )
            .map_err(|_| StoreError::Read)?;
        let rows = query
            .query_map(
                params![
                    phrase,
                    after.map(|key| bounded_id(key.id.0)),
                    i64::try_from(limit + 1).map_err(|_| StoreError::Read)?
                ],
                thread_row,
            )
            .map_err(|_| StoreError::Read)?;
        let rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Read)?;
        Ok(thread_result(rows, limit))
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
             AND e.kind NOT IN ('scene_begin', 'scene_cleared')
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
                        "revealed" => PageEventKind::Revealed,
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

fn cursor(row: &ThreadRow) -> ThreadCursor {
    ThreadCursor {
        updated_ms: row.updated_ms,
        id: row.id,
    }
}

fn thread_result(mut rows: Vec<ThreadRow>, limit: usize) -> (Vec<ThreadRow>, Option<ThreadCursor>) {
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = more.then(|| cursor(rows.last().expect("a full page has a last row")));
    (rows, next)
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
