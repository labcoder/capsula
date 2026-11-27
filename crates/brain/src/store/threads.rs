//! The main thread, and what the shell may change about a thread: a task's title and a thread's
//! model choice. Neither change moves a thread in the list.

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ModelChoice, SavedChoice, ThreadId, ThreadKind, ThreadTitle,
};
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};

use super::choice::{StoredChoice, connection_text, effort_text};
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

    /// Stores a live thread's model and effort, or clears them with `None` so each later run
    /// uses the global default, and advances its revision in the same transaction. Kumo's main
    /// chat takes this write like any task. Answers what was committed: `Missing` for a missing,
    /// hidden or unrepresentable thread, and `Failed` with nothing written once the revision
    /// can't advance.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn set_model(
        &mut self,
        thread: ThreadId,
        choice: Option<&ModelChoice>,
    ) -> Result<ChoiceRead, StoreError> {
        let Ok(thread) = sql_id(thread.0, StoreError::NoThread) else {
            return Ok(ChoiceRead::Missing);
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: Option<i64> = tx
            .query_row(
                "SELECT model_choice_rev FROM threads WHERE id = ?1 AND state = 'live'",
                params![thread],
                |row| row.get(0),
            )
            .optional()?;
        let Some(revision) = revision else {
            return Ok(ChoiceRead::Missing);
        };
        let Some((next, rev)) = revision
            .checked_add(1)
            .and_then(|next| Some((next, u64::try_from(next).ok()?)))
        else {
            tracing::warn!("a model choice's revision can't advance");
            return Ok(ChoiceRead::Failed);
        };
        tx.execute(
            "UPDATE threads SET model = ?2, effort = ?3, connection = ?4, model_choice_rev = ?5
             WHERE id = ?1",
            params![
                thread,
                choice.map(|choice| choice.model.as_str()),
                choice.map(|choice| effort_text(choice.effort)),
                choice.map(|choice| connection_text(choice.connection)),
                next
            ],
        )?;
        tx.commit()?;
        Ok(ChoiceRead::Found {
            choice: choice.map_or(SavedChoice::Inherit, |choice| {
                SavedChoice::Chosen(choice.clone())
            }),
            revision: ChoiceRev(rev),
        })
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

/// Displays a structurally valid saved selection without requiring a live catalog.
pub(super) fn choice_of(
    model: Option<&str>,
    effort: Option<&str>,
    connection: Option<&str>,
) -> rusqlite::Result<Option<ModelChoice>> {
    StoredChoice::decode(model, effort, connection).display()
}
