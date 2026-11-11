//! The tool-call ledger's one write (docs/development/tools.md).

use rusqlite::{OptionalExtension as _, params};

use super::{Store, StoreError, now_ms, sql_id};
use kumochi_protocol::message::ThreadId;

use crate::dispatch::{CommitCall, Committed, LocalWrite, ToolError};

impl Store {
    /// Applies a call's write and records it in the ledger, in one transaction, while its run's
    /// thread is live. `Failed`, with nothing written, when the thread is being deleted or gone;
    /// `Refused`, with nothing written, when the write's own check fails.
    /// Draft creation also requires the matching run to remain active in that thread.
    ///
    /// # Errors
    /// [`StoreError::Call`], with no SQLite text, when SQLite fails, the key is already
    /// recorded, or the output is over the column's cap; nothing is written.
    pub(crate) fn commit_call(&mut self, call: &CommitCall) -> Result<Committed, StoreError> {
        if let LocalWrite::CreateTask { thread, .. } = &call.write {
            self.reserve(*thread)?;
        }
        self.record_call(call).map_err(|error| match error {
            StoreError::Sql(_) => StoreError::Call,
            other => other,
        })
    }

    /// Raises the thread high-water to `thread` in its own commit, so the id is issued before its
    /// task exists and is never reused, even if the task never commits.
    ///
    /// # Errors
    /// [`StoreError::StaleId`] when `thread` isn't above the high-water; [`StoreError::Call`]
    /// when SQLite fails.
    pub(super) fn reserve(&mut self, thread: ThreadId) -> Result<(), StoreError> {
        let id = sql_id(thread.0, StoreError::StaleId)?;
        let reserved = self
            .conn
            .execute(
                "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'threads' AND seq < ?1",
                params![id],
            )
            .map_err(|_| StoreError::Call)?;
        if reserved == 0 {
            return Err(StoreError::StaleId);
        }
        Ok(())
    }

    fn record_call(&mut self, call: &CommitCall) -> Result<Committed, StoreError> {
        let (thread, run) = match (
            sql_id(call.thread.0, StoreError::NoThread),
            sql_id(call.key.run.0, StoreError::NoThread),
        ) {
            (Ok(thread), Ok(run)) => (thread, run),
            _ if matches!(&call.write, LocalWrite::DraftIssue { .. }) => {
                return Ok(Committed::Failed);
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
        };
        let tx = self.conn.transaction()?;
        match &call.write {
            LocalWrite::None => {}
            LocalWrite::Focus(target) => {
                let Ok(target) = sql_id(target.0, StoreError::Read) else {
                    return Ok(Committed::Refused(ToolError::NoTask));
                };
                let live = tx
                    .query_row(
                        "SELECT 1 FROM threads WHERE id = ?1 AND state = 'live'",
                        params![target],
                        |_| Ok(()),
                    )
                    .optional()?;
                if live.is_none() {
                    return Ok(Committed::Refused(ToolError::NoTask));
                }
            }
            LocalWrite::CreateTask {
                thread: task,
                title,
                ..
            } => {
                let now = now_ms();
                tx.execute(
                    "INSERT INTO threads (id, title, title_source, created_ms, updated_ms)
                     VALUES (?1, ?2, 'model', ?3, ?3)",
                    params![sql_id(task.0, StoreError::StaleId)?, title.as_str(), now],
                )?;
            }
            LocalWrite::DraftIssue {
                title,
                body,
                repo,
                account,
            } => {
                let active = tx
                    .query_row(
                        "SELECT 1 FROM runs r JOIN threads t ON t.id = r.thread
                         WHERE r.id = ?1 AND r.thread = ?2 AND r.state = 'active'
                         AND t.state = 'live'",
                        params![run, thread],
                        |_| Ok(()),
                    )
                    .optional()?;
                if active.is_none() {
                    return Ok(Committed::Failed);
                }
                super::actions::insert_draft(
                    &tx,
                    thread,
                    title,
                    body,
                    repo.as_ref(),
                    account.as_ref(),
                )?;
            }
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
            return Ok(Committed::Failed);
        }
        tx.commit()?;
        Ok(Committed::Done)
    }
}
