//! The context builder's read: one read transaction over a thread's conversation and every
//! source its history results name, so all of it comes from the same snapshot.

use std::fmt;

use kumochi_protocol::message::{EventId, ModelChoice, RunId, ThreadId, ThreadKind};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::threads::{choice_of, kind_of};
use super::{StoreError, row_id, sql_id};

/// A thread as a reference finds it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) kind: ThreadKind,
    /// False while it's being deleted.
    pub(crate) live: bool,
    pub(crate) title: String,
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("kind", &self.kind)
            .field("live", &self.live)
            .field("title_bytes", &self.title.len())
            .finish()
    }
}

/// Who wrote a stored message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Speaker {
    User,
    Assistant,
}

/// One stored message of a thread's conversation.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Said {
    pub(crate) id: EventId,
    pub(crate) run: RunId,
    pub(crate) speaker: Speaker,
    pub(crate) text: String,
}

impl fmt::Debug for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Said")
            .field("id", &self.id)
            .field("run", &self.run)
            .field("speaker", &self.speaker)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

/// What a range re-read found under its cap. Each event is one `speaker: text` line; the lines
/// that fit whole are kept, and an event that alone is over the cap keeps its longest prefix
/// that ends on a character boundary.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) text: String,
    /// The first and last events shown, the last perhaps cut; `None` when none was.
    pub(crate) shown: Option<(EventId, EventId)>,
    /// The last event shown was cut.
    pub(crate) cut: bool,
    /// The first event in the range not shown at all.
    pub(crate) more: Option<EventId>,
}

impl fmt::Debug for Excerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Excerpt")
            .field("text_bytes", &self.text.len())
            .field("shown", &self.shown)
            .field("cut", &self.cut)
            .field("more", &self.more)
            .finish()
    }
}

/// A read transaction, taken when it begins. It never writes.
#[derive(Debug)]
pub(crate) struct Snapshot<'a>(Transaction<'a>);

impl<'a> Snapshot<'a> {
    /// Begins the transaction and takes its snapshot at once, before anything else is read.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub(crate) fn begin(conn: &'a Connection) -> Result<Self, StoreError> {
        let tx = conn.unchecked_transaction()?;
        // A deferred transaction's snapshot starts at its first read, so this read starts it.
        tx.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        Ok(Self(tx))
    }
}

impl Snapshot<'_> {
    /// A thread by id; `None` once it's removed.
    ///
    /// # Errors
    /// Fails when SQLite fails or the stored kind is unknown.
    pub(crate) fn source(&self, thread: ThreadId) -> Result<Option<Source>, StoreError> {
        let Ok(id) = sql_id(thread.0, StoreError::NoThread) else {
            return Ok(None);
        };
        let found = self
            .0
            .query_row(
                "SELECT kind, state = 'live', title FROM threads WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Source {
                        kind: kind_of(row.get_ref(0)?.as_str()?)?,
                        live: row.get(1)?,
                        title: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    /// The thread's messages after `after`, oldest first. It reads newest first and stops at the
    /// first run boundary past `budget` bytes, since older runs couldn't fit the request.
    ///
    /// # Errors
    /// Fails when SQLite fails or a stored row is malformed.
    pub(crate) fn conversation(
        &self,
        thread: ThreadId,
        after: Option<EventId>,
        budget: usize,
    ) -> Result<Vec<Said>, StoreError> {
        let thread = sql_id(thread.0, StoreError::NoThread)?;
        let after = after.map_or(Ok(0), |id| sql_id(id.0, StoreError::Read))?;
        let mut statement = self.0.prepare(
            "SELECT id, run, kind, coalesce(text, '') FROM events
             WHERE thread = ?1 AND id > ?2 AND run IS NOT NULL
             AND kind IN ('user', 'assistant_text') ORDER BY id DESC",
        )?;
        let mut rows = statement.query(params![thread, after])?;
        let mut said: Vec<Said> = Vec::new();
        let mut bytes = 0_usize;
        while let Some(row) = rows.next()? {
            let run = RunId(row_id(row.get(1)?));
            if bytes > budget && said.last().is_some_and(|last| last.run != run) {
                break;
            }
            let kind: String = row.get(2)?;
            let speaker = if kind == "user" {
                Speaker::User
            } else {
                Speaker::Assistant
            };
            let text: String = row.get(3)?;
            bytes = bytes.saturating_add(text.len());
            said.push(Said {
                id: EventId(row_id(row.get(0)?)),
                run,
                speaker,
                text,
            });
        }
        said.reverse();
        Ok(said)
    }

    /// The messages of `thread` from `first` through `last`, under `cap` bytes. A live thread with
    /// nothing in the range gives an empty excerpt; liveness is [`Snapshot::source`]'s question.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub(crate) fn excerpt(
        &self,
        thread: ThreadId,
        first: EventId,
        last: EventId,
        cap: usize,
    ) -> Result<Excerpt, StoreError> {
        let mut excerpt = Excerpt::default();
        let (Ok(thread), Ok(first), Ok(last)) = (
            sql_id(thread.0, StoreError::Read),
            sql_id(first.0, StoreError::Read),
            sql_id(last.0, StoreError::Read),
        ) else {
            return Ok(excerpt);
        };
        let mut statement = self.0.prepare(
            "SELECT id, kind, coalesce(text, '') FROM events
             WHERE thread = ?1 AND id BETWEEN ?2 AND ?3
             AND kind IN ('user', 'assistant_text') ORDER BY id",
        )?;
        let mut rows = statement.query(params![thread, first, last])?;
        while let Some(row) = rows.next()? {
            let id = EventId(row_id(row.get(0)?));
            if excerpt.cut {
                excerpt.more = Some(id);
                break;
            }
            let kind: String = row.get(1)?;
            let text: String = row.get(2)?;
            let speaker = if kind == "user" { "user" } else { "assistant" };
            let line = format!("{speaker}: {text}");
            let start = excerpt.shown.map_or(id, |(start, _)| start);
            let separator = usize::from(!excerpt.text.is_empty());
            if excerpt.text.len() + separator + line.len() <= cap {
                if separator == 1 {
                    excerpt.text.push('\n');
                }
                excerpt.text.push_str(&line);
            } else if excerpt.text.is_empty() {
                excerpt.text.push_str(prefix(&line, cap));
                excerpt.cut = true;
            } else {
                excerpt.more = Some(id);
                break;
            }
            excerpt.shown = Some((start, id));
        }
        Ok(excerpt)
    }

    /// A live thread's stored model choice.
    ///
    /// # Errors
    /// Fails when SQLite fails or the stored model isn't a slug.
    pub(crate) fn choice(&self, thread: ThreadId) -> Result<Option<ModelChoice>, StoreError> {
        let Ok(id) = sql_id(thread.0, StoreError::NoThread) else {
            return Ok(None);
        };
        let choice = self
            .0
            .query_row(
                "SELECT model, effort FROM threads WHERE id = ?1 AND state = 'live'",
                params![id],
                |row| {
                    choice_of(
                        row.get_ref(0)?.as_str_or_null()?,
                        row.get_ref(1)?.as_str_or_null()?,
                    )
                },
            )
            .optional()?;
        Ok(choice.flatten())
    }

    /// The local date and time to the minute, from the system's time zone.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub(crate) fn local_time(&self) -> Result<String, StoreError> {
        let time = self.0.query_row(
            "SELECT strftime('%Y-%m-%d %H:%M', 'now', 'localtime')",
            [],
            |row| row.get(0),
        )?;
        Ok(time)
    }

    /// Ends the transaction.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub(crate) fn end(self) -> Result<(), StoreError> {
        self.0.commit()?;
        Ok(())
    }
}

/// The longest prefix of `text` within `cap` bytes that ends on a character boundary.
fn prefix(text: &str, cap: usize) -> &str {
    let mut end = cap.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
