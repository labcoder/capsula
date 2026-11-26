use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    Authority, AuthorityError, Current, GENERATIONS, Generation, Kind, VERSION, current,
    read_marker,
};

mod exchange;
pub(in crate::connections) use exchange::ExchangeStep;

const REPAIR_VERSION: i64 = 4;

const MIGRATION: &str = "
ALTER TABLE heads RENAME TO heads_v3;
ALTER TABLE generations RENAME TO generations_v3;
DROP INDEX one_active;
DROP INDEX one_pending;
DROP INDEX removed;
CREATE TABLE heads (
    kind TEXT PRIMARY KEY CHECK (kind IN ('host', 'chatgpt', 'revoke', 'anthropic', 'github', 'repair')),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;
INSERT INTO heads SELECT * FROM heads_v3;
INSERT INTO heads (kind, revision) VALUES ('repair', 0);
";

const REPAIR_SLOT: &str = "
INSERT INTO generations (rowid, generation, kind, state, chunks, bytes, deleted_in)
SELECT rowid, generation, kind, state, chunks, bytes, deleted_in FROM generations_v3;
DROP TABLE generations_v3;
DROP TABLE heads_v3;
CREATE TABLE chatgpt_repair (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    old_grant_notice INTEGER NOT NULL CHECK (old_grant_notice IN (0, 1)),
    fresh_text_only INTEGER NOT NULL CHECK (fresh_text_only IN (0, 1)),
    context_tag INTEGER CHECK (context_tag > 0),
    context_successor BLOB CHECK (length(context_successor) = 16),
    context_established INTEGER NOT NULL DEFAULT 0 CHECK (context_established IN (0, 1)),
    context_last_thread INTEGER CHECK (context_last_thread > 0),
    context_last_run INTEGER CHECK (context_last_run >= 0),
    recovered_sign_out INTEGER NOT NULL DEFAULT 0 CHECK (recovered_sign_out BETWEEN 0 AND 3),
    fenced BLOB REFERENCES generations (generation),
    phase TEXT NOT NULL CHECK (phase IN ('idle', 'browser', 'exchange', 'escrow', 'unknown')),
    attempt BLOB CHECK (length(attempt) = 16),
    opened_in INTEGER CHECK (opened_in >= 0),
    account_revision INTEGER CHECK (account_revision >= 0),
    host_revision INTEGER CHECK (host_revision >= 0),
    revocation_revision INTEGER CHECK (revocation_revision >= 0),
    credential BLOB REFERENCES generations (generation),
    candidate BLOB REFERENCES generations (generation),
    issued_by INTEGER CHECK (issued_by > 0),
    CHECK ((phase = 'idle') = (attempt IS NULL)),
    CHECK ((attempt IS NULL) = (opened_in IS NULL)),
    CHECK ((attempt IS NULL) = (account_revision IS NULL)),
    CHECK ((attempt IS NULL) = (host_revision IS NULL)),
    CHECK ((attempt IS NULL) = (revocation_revision IS NULL)),
    CHECK (attempt IS NULL OR fenced IS NOT NULL),
    CHECK (fenced IS NULL OR old_grant_notice = 1),
    CHECK (fresh_text_only = 0 OR old_grant_notice = 1),
    CHECK ((fresh_text_only = 1) = (context_tag IS NOT NULL)),
    CHECK ((context_tag IS NULL) = (context_successor IS NULL)),
    CHECK (context_established = 0 OR context_tag IS NOT NULL),
    CHECK ((context_established = 1) = (context_last_thread IS NOT NULL)),
    CHECK ((context_last_thread IS NULL) = (context_last_run IS NULL)),
    CHECK (recovered_sign_out = 0 OR (context_established = 1 AND phase = 'idle' AND fenced IS NULL)),
    CHECK (phase IN ('idle', 'browser') OR credential IS NOT NULL),
    CHECK (phase <> 'idle' OR credential IS NULL),
    CHECK (phase <> 'idle' OR candidate IS NULL),
    CHECK (phase IN ('escrow', 'unknown') OR issued_by IS NULL)
) STRICT;
INSERT INTO chatgpt_repair (id, old_grant_notice, fresh_text_only, phase) VALUES (1, 0, 0, 'idle');
";

#[derive(Debug)]
#[must_use]
pub(in crate::connections) struct RepairAttempt {
    id: Generation,
}

impl RepairAttempt {
    pub(in crate::connections) fn id(&self) -> Generation {
        self.id
    }
}

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

    pub(in crate::connections) fn repair_predecessor(&self) -> Result<Current, AuthorityError> {
        predecessor(&self.conn)
    }

    pub(in crate::connections) fn start_repair(
        &mut self,
        old: Current,
        host: Current,
        revocations: Current,
    ) -> Result<RepairAttempt, AuthorityError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let first = schema_version(&tx)? == VERSION;
        let old_ref = old.active.ok_or(AuthorityError::Conflict)?;
        let marker = read_marker(&tx)?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM generations WHERE kind IN ('chatgpt', 'revoke') AND state='pending'
            )",
            [],
            |row| row.get(0),
        )?;
        if old != predecessor(&tx)?
            || host.active.is_none()
            || host != current(&tx, Kind::Host)?
            || revocations != current(&tx, Kind::Revocations)?
            || marker.sign_out.is_some()
            || marker.request_pending
            || marker.sweep
            || pending
        {
            return Err(AuthorityError::Conflict);
        }
        let id = Generation::fresh()?;
        if first {
            tx.execute_batch(MIGRATION)?;
            tx.execute_batch(GENERATIONS)?;
            tx.execute_batch(REPAIR_SLOT)?;
            tx.pragma_update(None, "user_version", REPAIR_VERSION)?;
        }
        let updated = tx.execute(
            "UPDATE chatgpt_repair SET old_grant_notice=1, fenced=?1, phase='browser',
             attempt=?2, opened_in=?3, account_revision=?4, host_revision=?5, revocation_revision=?6
             WHERE id=1 AND phase='idle'",
            params![
                old_ref.generation.0.as_slice(),
                id.0.as_slice(),
                self.lifetime,
                sql_number(old.revision)?,
                sql_number(host.revision)?,
                sql_number(revocations.revision)?,
            ],
        )?;
        if updated != 1 {
            return Err(AuthorityError::Conflict);
        }
        let foreign_key_violation: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get(0),
        )?;
        if foreign_key_violation {
            return Err(AuthorityError::Corrupt);
        }
        self.account_fenced = true;
        if let Err(error) = tx.commit() {
            if first && matches!(schema_version(&self.conn), Ok(VERSION)) {
                self.account_fenced = false;
            }
            return Err(error.into());
        }
        Ok(RepairAttempt { id })
    }

    pub(in crate::connections) fn cancel_repair_admission(
        &mut self,
        attempt: &RepairAttempt,
    ) -> Result<(), AuthorityError> {
        if !self.account_fenced {
            return Err(AuthorityError::Conflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = attempt.id();
        let updated = tx.execute(
            "UPDATE chatgpt_repair SET phase='idle', attempt=NULL, opened_in=NULL,
             account_revision=NULL, host_revision=NULL, revocation_revision=NULL
             WHERE id=1 AND phase='browser' AND credential IS NULL AND candidate IS NULL
             AND attempt=?1 AND opened_in=?2",
            params![id.0.as_slice(), self.lifetime],
        )?;
        if updated != 1 {
            return Err(AuthorityError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }
}

fn predecessor(conn: &Connection) -> Result<Current, AuthorityError> {
    let version = schema_version(conn)?;
    let old = current(conn, Kind::Account)?;
    if version == REPAIR_VERSION {
        let active = old.active.ok_or(AuthorityError::Conflict)?;
        let available: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM chatgpt_repair WHERE id=1 AND phase='idle'
             AND fenced=?1 AND fresh_text_only=0 AND recovered_sign_out=0)",
            [active.generation.0.as_slice()],
            |row| row.get(0),
        )?;
        if !available {
            return Err(AuthorityError::Conflict);
        }
    } else if version != VERSION {
        return Err(AuthorityError::Conflict);
    }
    Ok(old)
}

fn schema_version(conn: &Connection) -> Result<i64, AuthorityError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn sql_number(value: u64) -> Result<i64, AuthorityError> {
    i64::try_from(value).map_err(|_| AuthorityError::Bounds)
}
