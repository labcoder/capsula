//! The tool-call ledger's one write (docs/development/tools.md).

use rusqlite::params;

use super::{Store, StoreError, now_ms, sql_id};
use crate::dispatch::{CommitCall, LocalWrite};

impl Store {
    /// Applies a call's write and records it in the ledger, in one transaction, while its run's
    /// thread is live. False, with nothing written, when the thread is being deleted or gone.
    ///
    /// # Errors
    /// [`StoreError::Call`], with no SQLite text, when SQLite fails, the key is already
    /// recorded, or the output is over the column's cap; nothing is written.
    pub(crate) fn commit_call(&mut self, call: &CommitCall) -> Result<bool, StoreError> {
        self.record_call(call).map_err(|error| match error {
            StoreError::Sql(_) => StoreError::Call,
            other => other,
        })
    }

    fn record_call(&mut self, call: &CommitCall) -> Result<bool, StoreError> {
        let thread = sql_id(call.thread.0, StoreError::NoThread)?;
        let run = sql_id(call.key.run.0, StoreError::NoThread)?;
        let tx = self.conn.transaction()?;
        match &call.write {
            LocalWrite::None => {}
            #[cfg(test)]
            LocalWrite::Title(title) => {
                tx.execute(
                    "UPDATE threads SET title = ?2 WHERE id = ?1",
                    params![thread, title],
                )?;
            }
        }
        let recorded = tx.execute(
            "INSERT INTO tool_calls (run, thread, route, call, tool, hash, result, created_ms)
             SELECT r.id, r.thread, ?3, ?4, ?5, ?6, ?7, ?8 FROM runs r
             JOIN threads t ON t.id = r.thread AND t.state = 'live'
             WHERE r.id = ?1 AND r.thread = ?2",
            params![
                run,
                thread,
                call.key.route.column(),
                &*call.key.call,
                call.tool,
                &call.hash[..],
                call.output.stored(),
                now_ms()
            ],
        )?;
        if recorded == 0 {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }
}
