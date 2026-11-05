//! The main thread, and what the shell may change about a thread: a task's title and a thread's
//! model choice. Neither change moves a thread in the list.

use kumochi_protocol::message::{
    Effort, ModelChoice, ModelSlug, ThreadId, ThreadKind, ThreadTitle,
};
use rusqlite::{OptionalExtension as _, params};

use super::{Generation, Store, StoreError, now_ms, row_id, sql_id};

/// Who set a task's title. A first-words title is `BeginTurn`'s, and only it can be replaced by
/// a model's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSource {
    /// The user's, which replaces any title.
    User,
    /// A model's, applied only over first words and only while the thread's deletion generation
    /// is still this one.
    Model(Generation),
}

impl Store {
    /// Creates the main thread if it's missing, in one statement of its own: a start cut short
    /// before it commits leaves none, and the next start creates it. The unique index keeps it one.
    pub(super) fn create_main(&self) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO threads (title, kind, created_ms, updated_ms)
             SELECT '', 'main', ?1, ?1 WHERE NOT EXISTS (SELECT 1 FROM threads WHERE kind = 'main')",
            params![now_ms()],
        )?;
        Ok(())
    }

    /// The main thread, which [`Store::recover`] creates.
    ///
    /// # Errors
    /// Fails with [`StoreError::NoThread`] before the recovery scan has run, and when SQLite fails.
    pub fn main_thread(&self) -> Result<ThreadId, StoreError> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM threads WHERE kind = 'main'", [], |row| {
                row.get(0)
            })
            .optional()?;
        id.map(|id| ThreadId(row_id(id)))
            .ok_or(StoreError::NoThread)
    }

    /// Retitles a live task, and says whether it did. The main thread's stored title stays
    /// empty, and a title that can't apply (a missing, hidden or main thread, or a model title
    /// the rules turn away) changes nothing.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn set_title(
        &mut self,
        thread: ThreadId,
        title: &ThreadTitle,
        source: TitleSource,
    ) -> Result<bool, StoreError> {
        let Ok(thread) = sql_id(thread.0, StoreError::NoThread) else {
            return Ok(false);
        };
        let (source, generation) = match source {
            TitleSource::User => ("user", None),
            TitleSource::Model(Generation(generation)) => ("model", Some(generation)),
        };
        let changed = self.conn.execute(
            "UPDATE threads SET title = ?2, title_source = ?3
             WHERE id = ?1 AND state = 'live' AND kind = 'thread'
             AND (?4 IS NULL OR (title_source = 'words' AND deletion_generation = ?4))",
            params![thread, title.as_str(), source, generation],
        )?;
        Ok(changed > 0)
    }

    /// Stores a live thread's model and effort, returning whether a live row matched.
    /// A missing, hidden or unrepresentable thread returns false without changing anything.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn set_model(
        &mut self,
        thread: ThreadId,
        choice: &ModelChoice,
    ) -> Result<bool, StoreError> {
        let Ok(thread) = sql_id(thread.0, StoreError::NoThread) else {
            return Ok(false);
        };
        let changed = self.conn.execute(
            "UPDATE threads SET model = ?2, effort = ?3 WHERE id = ?1 AND state = 'live'",
            params![thread, choice.model.as_str(), effort_text(choice.effort)],
        )?;
        Ok(changed > 0)
    }
}

fn effort_text(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// A stored `kind`. One no build wrote fails the read rather than being guessed at.
pub(super) fn kind_of(text: &str) -> rusqlite::Result<ThreadKind> {
    match text {
        "main" => Ok(ThreadKind::Main),
        "thread" => Ok(ThreadKind::Thread),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

/// A stored `model` and `effort`, which are both set or both null. A slug no build wrote fails the
/// read. An effort this build doesn't know, an older or a newer build's, reads as no choice, so the
/// thread falls back to the default model and the page still reads (CAPS-90 M1).
pub(super) fn choice_of(
    model: Option<&str>,
    effort: Option<&str>,
) -> rusqlite::Result<Option<ModelChoice>> {
    let (Some(model), Some(effort)) = (model, effort) else {
        return Ok(None);
    };
    let model = ModelSlug::new(model).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let Some(effort) = Effort::ALL
        .into_iter()
        .find(|known| effort_text(*known) == effort)
    else {
        tracing::warn!("a stored effort this build doesn't know reads as no choice");
        return Ok(None);
    };
    // shortcut: the store keeps no connection until A3's migration, so every saved choice reads
    // as ChatGPT's.
    Ok(Some(ModelChoice {
        model,
        effort,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }))
}
