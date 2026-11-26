use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use super::super::{
    ActiveRef, Authority, AuthorityError, Generation, Kind, Layout, Verified, WritePlan,
    activate_plan, current, head_revision, read_marker,
};
use super::RepairAttempt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::connections) enum ExchangeStep {
    Charge,
    Drain,
}

impl ExchangeStep {
    fn phase(self) -> &'static str {
        match self {
            Self::Charge => "browser",
            Self::Drain => "exchange",
        }
    }
}

impl Authority {
    pub(in crate::connections) fn stage_repair_credential(
        &mut self,
        attempt: &RepairAttempt,
        step: ExchangeStep,
        len: usize,
    ) -> Result<WritePlan, AuthorityError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if candidate(&tx, self.lifetime, attempt, step)?.is_some() {
            return Err(AuthorityError::Conflict);
        }
        let layout = Layout::for_len(len)?;
        let (chunks, bytes) = layout.sql()?;
        let generation = Generation::fresh()?;
        let kind = Kind::Repair;
        let base = head_revision(&tx, kind)?;
        tx.execute(
            "INSERT INTO generations (generation, kind, state, chunks, bytes)
             VALUES (?1, ?2, 'pending', ?3, ?4)",
            params![generation.0.as_slice(), kind.label(), chunks, bytes],
        )?;
        tx.execute(
            "UPDATE chatgpt_repair SET candidate=?1 WHERE id=1",
            [generation.0.as_slice()],
        )?;
        tx.commit()?;
        Ok(WritePlan {
            kind,
            generation,
            layout,
            base,
        })
    }

    pub(in crate::connections) fn commit_repair_credential(
        &mut self,
        attempt: &RepairAttempt,
        step: ExchangeStep,
        verified: Verified,
    ) -> Result<(), AuthorityError> {
        let next = match step {
            ExchangeStep::Charge => "exchange",
            ExchangeStep::Drain => "escrow",
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let plan = verified.into_plan();
        if plan.kind != Kind::Repair
            || candidate(&tx, self.lifetime, attempt, step)? != Some(plan.generation)
        {
            return Err(AuthorityError::Conflict);
        }
        let active = activate_plan(&tx, &plan)?;
        tx.execute(
            "UPDATE chatgpt_repair SET phase=?1, credential=?2, candidate=NULL WHERE id=1",
            params![next, active.generation.0.as_slice()],
        )?;
        tx.commit()?;
        Ok(())
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

    pub(in crate::connections) fn charged_repair_client(
        &self,
        attempt: &RepairAttempt,
    ) -> Result<ActiveRef, AuthorityError> {
        candidate(&self.conn, self.lifetime, attempt, ExchangeStep::Drain)?;
        current(&self.conn, Kind::Repair)?
            .active
            .ok_or(AuthorityError::Corrupt)
    }

    pub(in crate::connections) fn settle_repair_uncertain(
        &mut self,
        attempt: &RepairAttempt,
    ) -> Result<(), AuthorityError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        candidate(&tx, self.lifetime, attempt, ExchangeStep::Drain)?;
        tx.execute(
            "UPDATE chatgpt_repair SET phase='unknown', opened_in=0 WHERE id=1",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(in crate::connections) fn seal_repair_exchange(
        &mut self,
        attempt: &RepairAttempt,
    ) -> Result<(), AuthorityError> {
        if !self.account_fenced {
            return Err(AuthorityError::Conflict);
        }
        let id = attempt.id();
        let updated = self.conn.execute(
            "UPDATE chatgpt_repair SET opened_in=0 WHERE id=1 AND attempt=?1 AND opened_in=?2
             AND (phase IN ('exchange', 'escrow', 'unknown') OR candidate IS NOT NULL)",
            params![id.0.as_slice(), self.lifetime],
        )?;
        if updated != 1 {
            return Err(AuthorityError::Conflict);
        }
        Ok(())
    }
}

fn candidate(
    conn: &Connection,
    lifetime: i64,
    attempt: &RepairAttempt,
    step: ExchangeStep,
) -> Result<Option<Generation>, AuthorityError> {
    let id = attempt.id();
    let (candidate, credential): (Option<Vec<u8>>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT candidate, credential FROM chatgpt_repair
             WHERE id=1 AND attempt=?1 AND phase=?2 AND
               (opened_in=?3 OR (phase='exchange' AND opened_in=0))",
            params![id.0.as_slice(), step.phase(), lifetime],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(AuthorityError::Conflict)?;
    let credential = credential
        .as_deref()
        .map(Generation::from_sql)
        .transpose()?;
    if credential
        != current(conn, Kind::Repair)?
            .active
            .map(|active| active.generation)
        || (step == ExchangeStep::Charge) != credential.is_none()
    {
        return Err(AuthorityError::Conflict);
    }
    if step != ExchangeStep::Drain {
        let marker = read_marker(conn)?;
        let same_heads: bool = conn.query_row(
            "SELECT EXISTS (
               SELECT 1 FROM chatgpt_repair AS r
               JOIN generations AS g ON g.generation=r.fenced AND g.kind='chatgpt' AND g.state='active'
               JOIN heads AS a ON a.kind='chatgpt' AND a.revision=r.account_revision
               JOIN heads AS h ON h.kind='host' AND h.revision=r.host_revision
               JOIN heads AS v ON v.kind='revoke' AND v.revision=r.revocation_revision
               WHERE r.id=1 AND NOT EXISTS (
                 SELECT 1 FROM generations AS p WHERE p.state='pending' AND
                  (p.kind='revoke' OR (p.kind IN ('chatgpt', 'repair') AND p.generation IS NOT r.candidate))
               )
             )",
            [],
            |row| row.get(0),
        )?;
        if !same_heads
            || marker.sign_out.is_some()
            || marker.request_pending
            || marker.sweep
        {
            return Err(AuthorityError::Conflict);
        }
    }
    candidate.as_deref().map(Generation::from_sql).transpose()
}
