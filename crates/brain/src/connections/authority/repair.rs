use rusqlite::{Connection, OptionalExtension as _};

use super::{Authority, AuthorityError, Kind, VERSION, current, read_marker};

impl Authority {
    /// Whether `ChatGPT`'s account can only be an ordinary one: the authority is at the ordinary
    /// schema, which no repair has reached. A repaired schema is never ordinary, whatever its row
    /// says, and a connection still inside a transaction answers nothing.
    pub(in crate::connections) fn ordinary_account(&self) -> Result<bool, AuthorityError> {
        if !self.conn.is_autocommit() {
            return Err(AuthorityError::Conflict);
        }
        Ok(schema_version(&self.conn)? == VERSION)
    }

    /// Whether the recovered account is signing out, by its own row or a pending sign-out.
    pub(in crate::connections) fn recovered_signing_out(&self) -> Result<bool, AuthorityError> {
        let row: i64 = self.conn.query_row(
            "SELECT recovered_sign_out FROM chatgpt_repair WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        Ok(row != 0 || read_marker(&self.conn)?.sign_out.is_some())
    }

    /// Reads only committed state: a connection still inside a transaction, as after a COMMIT
    /// whose rollback also failed, is never established.
    pub(in crate::connections) fn established_repair_context(
        &self,
    ) -> Result<(u64, (i64, i64)), AuthorityError> {
        if !self.conn.is_autocommit() {
            return Err(AuthorityError::Conflict);
        }
        let active = current(&self.conn, Kind::Account)?
            .active
            .ok_or(AuthorityError::Conflict)?;
        let (tag, last_thread, last_run): (i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT context_tag, context_last_thread, context_last_run FROM chatgpt_repair
                 WHERE id=1 AND phase='idle' AND fenced IS NULL AND fresh_text_only=1
                 AND context_established=1 AND context_successor=?1",
                [active.generation.0.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(AuthorityError::Conflict)?;
        let tag = u64::try_from(tag).map_err(|_| AuthorityError::Corrupt)?;
        Ok((tag, (last_thread, last_run)))
    }
}

fn schema_version(conn: &Connection) -> Result<i64, AuthorityError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}
