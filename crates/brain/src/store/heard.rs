//! A spoken run's heard point (docs/development/voice.md), written once the run is released.

use rusqlite::params;

use super::{Store, StoreError, sql_id};
use crate::voice::reply::Heard;

impl Store {
    /// Records how far the user heard an ended run of a live thread. It only moves the point
    /// forward, and an active, missing or deleted run is left as it is.
    ///
    /// # Errors
    /// Fails when SQLite does; nothing is written.
    pub(crate) fn heard(
        &mut self,
        run: kumochi_protocol::message::RunId,
        heard: Heard,
    ) -> Result<(), StoreError> {
        let run = sql_id(run.0, StoreError::NoRun)?;
        self.conn.execute(
            "UPDATE runs SET heard_segment = ?2, heard_sample = ?3, heard_text = ?4
             WHERE id = ?1 AND state <> 'active'
               AND thread IN (SELECT id FROM threads WHERE state = 'live')
               AND (heard_segment IS NULL OR (heard_segment, heard_sample) < (?2, ?3))",
            params![
                run,
                heard.segment.0,
                heard.sample,
                i64::try_from(heard.text).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }
}
