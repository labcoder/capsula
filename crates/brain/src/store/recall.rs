//! Live recall in the context snapshot; errors discard SQLite's content-bearing diagnostics.

use std::collections::BTreeSet;

use kumochi_protocol::message::{ThreadId, ThreadKind};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::context::Snapshot;
use super::{StoreError, row_id};
use crate::context::{Origin, REMOVED, UNAVAILABLE};
use crate::recall::{
    CUT, Error, Hit, MORE, Query, READ_EVENTS, SEARCH_HITS, SNIPPET_BYTES, Selection, TASKS,
    TEXT_BYTES, words,
};

struct Event {
    id: u64,
    speaker: String,
    text: String,
}

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, Error> {
    result.map_err(|_| Error::Read)
}

fn own(origin: Origin) -> Option<u64> {
    match origin {
        Origin::Kumo => None,
        Origin::Task(task) => Some(task.0),
    }
}

fn key(id: u64) -> Result<i64, Error> {
    i64::try_from(id).map_err(|_| Error::Invalid)
}

fn reach(origin: Origin) -> Result<Option<i64>, Error> {
    own(origin).map(key).transpose()
}

fn range(task: u64, events: &[Value], cut: Option<usize>, next: Option<u64>) -> Value {
    json!({"task":task,"events":events,"cut":cut.is_some(),"next":next,"note":cut.map(|_|CUT)})
}

fn event(row: &Event, text: &str) -> Value {
    let speaker = if row.speaker == "assistant_text" {
        "assistant"
    } else {
        "user"
    };
    json!({"event":row.id,"speaker":speaker,"text":text})
}

fn prefix(text: &str, fits: impl Fn(&str) -> bool) -> usize {
    if fits(text) {
        return text.len();
    }
    let (mut lo, mut hi) = (0, text.len());
    while lo < hi {
        let mut end = lo + (hi - lo) / 2;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == lo {
            end += text[lo..]
                .chars()
                .next()
                .expect("lo precedes hi within the text")
                .len_utf8();
        }
        if end == hi {
            break;
        }
        if fits(&text[..end]) {
            lo = end;
        } else {
            hi = end;
        }
    }
    lo
}

impl Snapshot<'_> {
    fn live_task(&self, task: u64, origin: Origin) -> Result<bool, Error> {
        if own(origin).is_some_and(|own| own != task) {
            return Err(Error::NotHere);
        }
        let source = self.source(ThreadId(task)).map_err(|_| Error::Read)?;
        if source
            .as_ref()
            .is_some_and(|source| source.kind == ThreadKind::Main)
        {
            return Err(Error::NotHere);
        }
        Ok(source.is_some_and(|source| source.live && source.kind == ThreadKind::Thread))
    }

    fn recall_events(
        &self,
        task: u64,
        from: u64,
        to: u64,
        origin: Origin,
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        let mut statement = sql(self.0.prepare(
            "SELECT e.id, e.kind, e.text FROM events e JOIN threads t ON t.id=e.thread
             WHERE e.thread=?1 AND e.id BETWEEN ?2 AND ?3
             AND (?4 IS NULL OR e.thread=?4) AND t.state='live' AND t.kind='thread'
             AND e.kind IN ('user','assistant_text') AND e.text IS NOT NULL
             ORDER BY e.id LIMIT ?5",
        ))?;
        let rows = sql(statement.query_map(
            params![
                key(task)?,
                key(from)?,
                key(to)?,
                reach(origin)?,
                i64::try_from(limit + 1).map_err(|_| Error::Invalid)?
            ],
            |row| {
                Ok(Event {
                    id: row_id(row.get(0)?),
                    speaker: row.get(1)?,
                    text: row.get(2)?,
                })
            },
        ))?;
        sql(rows.collect())
    }

    pub(crate) fn select_recall(&self, origin: Origin, query: &Query) -> Result<Selection, Error> {
        query.validate()?;
        match query {
            Query::Search { query, task } => {
                if let Some(task) = task
                    && !self.live_task(*task, origin)?
                {
                    return Err(Error::NoTask);
                }
                let mut statement = sql(self.0.prepare(
                    "SELECT e.thread,e.id FROM events_fts JOIN events e ON e.id=events_fts.rowid
                     JOIN threads t ON t.id=e.thread WHERE events_fts MATCH ?1
                     AND (?2 IS NULL OR e.thread=?2) AND (?3 IS NULL OR e.thread=?3)
                     AND t.state='live' AND t.kind='thread' AND e.kind IN ('user','assistant_text')
                     ORDER BY bm25(events_fts),e.id DESC LIMIT ?4",
                ))?;
                let rows = sql(statement.query_map(
                    params![
                        words(query)?,
                        task.map(key).transpose()?,
                        reach(origin)?,
                        i64::try_from(SEARCH_HITS + 1).map_err(|_| Error::Invalid)?,
                    ],
                    |row| {
                        Ok(Hit {
                            task: row_id(row.get(0)?),
                            event: row_id(row.get(1)?),
                        })
                    },
                ))?;
                let mut hits: Vec<Hit> = sql(rows.collect())?;
                let more = hits.len() > SEARCH_HITS;
                hits.truncate(SEARCH_HITS);
                Ok(Selection::Search {
                    query: query.clone(),
                    hits,
                    more,
                })
            }
            Query::Read { task, from, to } => {
                if !self.live_task(*task, origin)? {
                    return Err(Error::NoTask);
                }
                let tuning = self.tuning();
                let limit = tuning.read_events.min(READ_EVENTS);
                let bytes_limit = tuning.reread.min(TEXT_BYTES);
                let rows = self.recall_events(*task, *from, *to, origin, limit)?;
                let mut shown = Vec::new();
                let mut cut = None;
                let mut next = None;
                for (at, row) in rows.iter().enumerate() {
                    if at == limit {
                        next = Some(row.id);
                        break;
                    }
                    shown.push(event(row, &row.text));
                    let following = rows.get(at + 1).map(|row| row.id);
                    if range(*task, &shown, None, following).to_string().len() > bytes_limit {
                        shown.pop();
                        if at == 0 {
                            let bytes = prefix(&row.text, |text| {
                                range(*task, &[event(row, text)], Some(text.len()), following)
                                    .to_string()
                                    .len()
                                    <= bytes_limit
                            });
                            cut = Some(bytes);
                            shown.push(event(row, &row.text[..bytes]));
                            next = following;
                        } else {
                            next = Some(row.id);
                        }
                        break;
                    }
                }
                let first_shown = (!shown.is_empty()).then(|| rows[0].id);
                let last_shown = shown.len().checked_sub(1).map(|last| rows[last].id);
                Ok(Selection::Read {
                    task: *task,
                    first_shown,
                    last_shown,
                    cut,
                    next,
                })
            }
            Query::Tasks => self.select_tasks(origin),
        }
    }

    fn select_tasks(&self, origin: Origin) -> Result<Selection, Error> {
        if origin != Origin::Kumo {
            return Err(Error::NotHere);
        }
        let mut statement = sql(self.0.prepare(
            "SELECT id FROM threads
             WHERE kind='thread' AND state='live'
             ORDER BY updated_ms DESC,id DESC LIMIT ?1",
        ))?;
        let rows = sql(statement.query_map(
            params![i64::try_from(TASKS + 1).map_err(|_| Error::Invalid)?],
            |row| row.get::<_, i64>(0).map(row_id),
        ))?;
        let mut tasks: Vec<u64> = sql(rows.collect())?;
        let mut truncated = tasks.len() > TASKS;
        tasks.truncate(TASKS);
        let mut metadata = Vec::new();
        for task in &tasks {
            metadata.push(self.metadata(*task)?);
            if json!({
                "quoted_history": metadata,
                "note": null,
                "duplicate_titles": false,
                "truncated": false
            })
            .to_string()
            .len()
                > TEXT_BYTES
            {
                metadata.pop();
                truncated = true;
                break;
            }
        }
        tasks.truncate(metadata.len());
        Ok(Selection::Tasks { tasks, truncated })
    }

    fn metadata(&self, task: u64) -> Result<Value, Error> {
        sql(self.0.query_row(
            "SELECT t.title,t.updated_ms,r.state,
             EXISTS(SELECT 1 FROM runs a WHERE a.thread=t.id AND a.state='active')
             FROM threads t LEFT JOIN runs r
             ON r.id=(SELECT max(id) FROM runs WHERE thread=t.id)
             WHERE t.id=?1",
            [key(task)?],
            |row| {
                let state: Option<String> = row.get(2)?;
                let outcome = match state.as_deref() {
                    Some("done") => Some("Done"),
                    Some("interrupted") => Some("Stopped"),
                    Some("failed") => Some("Failed"),
                    _ => None,
                };
                Ok(json!({
                    "task": task,
                    "title": row.get::<_, String>(0)?,
                    "kind": "thread",
                    "working": row.get::<_, bool>(3)?,
                    "outcome": outcome,
                    "last_use": row.get::<_, i64>(1)?
                }))
            },
        ))
    }

    pub(crate) fn render_recall(
        &self,
        origin: Origin,
        selection: &Selection,
        sources: &mut BTreeSet<ThreadId>,
    ) -> Result<String, StoreError> {
        let mut read = || {
            self.render_selected(origin, selection, sources)
                .map(|value| value.to_string())
        };
        match &self.1 {
            Some(control) => control.query(&self.0, read),
            None => read(),
        }
        .map_err(StoreError::from)
    }

    pub(crate) fn recall_call(
        &self,
        origin: Origin,
        query: &Query,
    ) -> Result<(Selection, Vec<ThreadId>), Error> {
        let selection = self.select_recall(origin, query)?;
        selection.validate()?;
        let mut sources = BTreeSet::new();
        self.render_selected(origin, &selection, &mut sources)?;
        Ok((selection, sources.into_iter().collect()))
    }

    fn render_selected(
        &self,
        origin: Origin,
        selection: &Selection,
        sources: &mut BTreeSet<ThreadId>,
    ) -> Result<Value, Error> {
        selection.validate()?;
        let mut quoted = Vec::new();
        let (tasks, note): (Vec<u64>, _) = match selection {
            Selection::Search { hits, more, .. } => (
                hits.iter().map(|hit| hit.task).collect(),
                more.then_some(MORE),
            ),
            Selection::Read { task, .. } => (vec![*task], None),
            Selection::Tasks { tasks, .. } => (tasks.clone(), None),
        };
        for (at, task) in tasks.into_iter().enumerate() {
            let available = self.live_task(task, origin);
            if matches!(selection, Selection::Tasks { .. }) && origin != Origin::Kumo
                || matches!(available, Err(Error::NotHere))
            {
                quoted.push(json!({"task":task,"note":UNAVAILABLE}));
                continue;
            }
            if !available? {
                quoted.push(json!({"task":task,"note":REMOVED}));
                continue;
            }
            let mut value = match selection {
                Selection::Search { query, hits, .. } => {
                    let found = sql(self
                        .0
                        .query_row(
                            "SELECT snippet(events_fts,0,'','',' … ',24) FROM events_fts
                         JOIN events e ON e.id=events_fts.rowid WHERE events_fts MATCH ?1
                         AND e.id=?2 AND e.thread=?3 AND e.kind IN ('user','assistant_text')",
                            params![words(query)?, key(hits[at].event)?, key(task)?],
                            |row| row.get::<_, String>(0),
                        )
                        .optional())?;
                    let Some(text) = found else {
                        quoted.push(json!({"task":task,"note":UNAVAILABLE}));
                        continue;
                    };
                    let bytes =
                        prefix(&text, |text| json!(text).to_string().len() <= SNIPPET_BYTES);
                    json!({"task":task,"event":hits[at].event,"text":&text[..bytes]})
                }
                Selection::Read {
                    first_shown,
                    last_shown,
                    cut,
                    next,
                    ..
                } => {
                    let mut events = Vec::new();
                    if let (Some(first), Some(last)) = (first_shown, last_shown) {
                        let rows = self.recall_events(task, *first, *last, origin, READ_EVENTS)?;
                        if rows.len() > READ_EVENTS {
                            return Err(Error::Invalid);
                        }
                        for row in rows {
                            let text = if Some(row.id) == *last_shown
                                && let Some(bytes) = cut
                            {
                                row.text.get(..*bytes).ok_or(Error::Invalid)?
                            } else {
                                &row.text
                            };
                            events.push(event(&row, text));
                        }
                    }
                    let value = range(task, &events, *cut, *next);
                    if value.to_string().len() > TEXT_BYTES {
                        return Err(Error::Invalid);
                    }
                    value
                }
                Selection::Tasks { .. } => self.metadata(task)?,
            };
            let source = self
                .source(ThreadId(task))
                .map_err(|_| Error::Read)?
                .ok_or(Error::NoTask)?;
            sources.insert(ThreadId(task));
            value["title"] = json!(source.title);
            quoted.push(value);
        }
        let duplicate_titles = quoted.iter().enumerate().any(|(at, item)| {
            item.get("title").is_some_and(|title| {
                quoted[..at].iter().any(|other| {
                    other.get("title") == Some(title) && other.get("task") != item.get("task")
                })
            })
        });
        Ok(json!({
            "quoted_history": quoted,
            "note": note,
            "duplicate_titles": duplicate_titles,
            "truncated": matches!(selection, Selection::Tasks { truncated: true, .. })
        }))
    }
}

#[cfg(test)]
mod tests;
