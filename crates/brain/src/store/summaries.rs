//! Summaries: compaction's one write (docs/development/store.md, "Summaries").

use kumochi_protocol::message::{EventId, ThreadId};
use rusqlite::params;

use super::{Generation, Store, StoreError, now_ms, sql_id};

impl Store {
    /// Stores a summary of `thread`'s events `first` through `last`, only while the thread is
    /// live and still in the deletion generation the summary was read in, so a delete during
    /// compaction stores nothing. True when it was stored.
    ///
    /// # Errors
    /// Fails when SQLite fails.
    pub fn store_summary(
        &mut self,
        thread: ThreadId,
        generation: Generation,
        (first, last): (EventId, EventId),
        text: &str,
        model: Option<&str>,
    ) -> Result<bool, StoreError> {
        let (Ok(thread), Ok(first), Ok(last)) = (
            sql_id(thread.0, StoreError::NoThread),
            sql_id(first.0, StoreError::Read),
            sql_id(last.0, StoreError::Read),
        ) else {
            return Ok(false);
        };
        let stored = self.conn.execute(
            "INSERT INTO summaries (thread, first_event, last_event, text, model, created_ms)
             SELECT id, ?3, ?4, ?5, ?6, ?7 FROM threads
             WHERE id = ?1 AND state = 'live' AND deletion_generation = ?2",
            params![thread, generation.0, first, last, text, model, now_ms()],
        )?;
        Ok(stored > 0)
    }
}
