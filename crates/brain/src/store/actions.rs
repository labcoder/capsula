//! Issue actions' rows (docs/development/issues.md): one transaction per change, live threads
//! only, and a fresh approval revision with every stage that offers a decision.

use kumochi_protocol::message::{
    ApprovalRev, DraftId, GitHubLogin, IssueBody, IssueRefused, IssueTitle, RepoName, StateVersion,
    ThreadId,
};
use rusqlite::{Connection, OptionalExtension as _, Row, Transaction, params};

use super::{Store, StoreError, now_ms, sql_id};
use crate::connections::github::api::GitHubUser;
use crate::issues::{Action, Answer, Stage};

impl Store {
    /// Stores a new draft in a live thread, with its first approval revision.
    ///
    /// # Errors
    /// [`StoreError::NoThread`] when the thread isn't live; [`StoreError::Issue`] when the write
    /// fails, with nothing written.
    pub fn create_draft(
        &mut self,
        thread: ThreadId,
        title: &IssueTitle,
        body: &IssueBody,
        repo: Option<&RepoName>,
        account: Option<&GitHubUser>,
    ) -> Result<Action, StoreError> {
        let thread = sql_id(thread.0, StoreError::NoThread)?;
        self.action_tx(|tx| {
            let approval = mint(tx)?;
            let inserted = tx.execute(
                "INSERT INTO actions (thread, version, stage, repo, account_id, account_login, title,
                 body, approval, created_ms)
                 SELECT id, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9 FROM threads
                 WHERE id = ?1 AND state = 'live'",
                params![
                    thread,
                    stage_json(&Stage::Draft)?,
                    repo.map(RepoName::as_str),
                    account.map(|user| user.id.to_string()),
                    account.map(|user| user.login.as_str()),
                    title.as_str(),
                    body.as_str(),
                    approval,
                    now_ms()
                ],
            )?;
            if inserted == 0 {
                return Err(StoreError::NoThread);
            }
            load(tx, tx.last_insert_rowid())?.ok_or(StoreError::Issue)
        })
    }

    /// The action, while its thread is live.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the read fails or the row doesn't decode.
    pub fn action(&self, draft: DraftId) -> Result<Option<Action>, StoreError> {
        let Ok(id) = i64::try_from(draft.0) else {
            return Ok(None);
        };
        load(&self.conn, id).map_err(quiet)
    }

    /// Replaces a Draft's or Failed action's title, body and repository together.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn edit_draft(
        &mut self,
        draft: DraftId,
        title: &IssueTitle,
        body: &IssueBody,
        repo: Option<&RepoName>,
    ) -> Result<Answer, StoreError> {
        self.change(draft, |action| action.edit(title, body, repo))
    }

    /// Dismisses an action that isn't sending or settled.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn discard_draft(&mut self, draft: DraftId) -> Result<Answer, StoreError> {
        self.change(draft, Action::discard)
    }

    /// Ends the sends a crash cut short in live threads, as [`Stage::interrupted`] says, reading
    /// only each row's stage. Returns how many failed and how many became Unknown.
    pub(super) fn recover_sends(&mut self) -> Result<(usize, usize), StoreError> {
        self.action_tx(|tx| {
            let rows: Vec<(i64, String, u32)> = tx
                .prepare(
                    "SELECT a.id, a.stage, a.uncertain FROM actions a
                     JOIN threads t ON t.id = a.thread AND t.state = 'live'
                     WHERE json_extract(a.stage, '$.kind') IN ('preflight', 'posting')",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?;
            let mut failed = 0;
            for (id, stage, uncertain) in &rows {
                let stage: Stage = serde_json::from_str(stage).map_err(|_| StoreError::Issue)?;
                let (stage, uncertain) = stage.interrupted(*uncertain).ok_or(StoreError::Issue)?;
                failed += usize::from(matches!(stage, Stage::Failed { .. }));
                commit_stage(tx, *id, &stage, uncertain)?;
            }
            Ok((failed, rows.len() - failed))
        })
    }

    /// Applies one transition to the row reloaded in its own transaction, and writes only a
    /// change.
    fn change(
        &mut self,
        draft: DraftId,
        apply: impl FnOnce(&mut Action) -> Result<(), IssueRefused>,
    ) -> Result<Answer, StoreError> {
        let Ok(id) = i64::try_from(draft.0) else {
            return Ok(Answer::Gone);
        };
        self.action_tx(|tx| {
            let Some(before) = load(tx, id)? else {
                return Ok(Answer::Gone);
            };
            let mut after = before.clone();
            if let Err(why) = apply(&mut after) {
                return Ok(Answer::Refused(why, before));
            }
            if after == before {
                return Ok(Answer::Unchanged(before));
            }
            tx.execute(
                "UPDATE actions SET repo = ?2, title = ?3, body = ?4 WHERE id = ?1",
                params![
                    id,
                    after.repo.as_ref().map(RepoName::as_str),
                    after.title.as_str(),
                    after.body.as_str()
                ],
            )?;
            commit_stage(tx, id, &after.stage, after.uncertain)?;
            Ok(Answer::Changed(load(tx, id)?.ok_or(StoreError::Issue)?))
        })
    }

    /// Runs `op` in one transaction, committed only when it succeeds.
    fn action_tx<T>(
        &mut self,
        op: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = self.conn.transaction().map_err(quiet)?;
        let value = op(&tx).map_err(quiet)?;
        tx.commit().map_err(quiet)?;
        Ok(value)
    }
}

/// SQLite's text can quote a row's content, so it never leaves an action op.
fn quiet(error: impl Into<StoreError>) -> StoreError {
    match error.into() {
        StoreError::Sql(_) => StoreError::Issue,
        other => other,
    }
}

/// Moves a row to `stage`, raising its version, with a fresh approval where the stage offers a
/// decision and none elsewhere.
fn commit_stage(tx: &Connection, id: i64, stage: &Stage, uncertain: u32) -> Result<(), StoreError> {
    let approval = matches!(
        stage,
        Stage::Draft | Stage::Failed { .. } | Stage::Unknown { .. }
    )
    .then(|| mint(tx))
    .transpose()?;
    let moved = tx.execute(
        "UPDATE actions SET stage = ?2, uncertain = ?3, approval = ?4, version = version + 1
         WHERE id = ?1 AND version < 9223372036854775807",
        params![id, stage_json(stage)?, uncertain, approval],
    )?;
    if moved == 1 {
        Ok(())
    } else {
        Err(StoreError::Issue)
    }
}

/// The next approval revision. The mark outlives deletions and purges, so none is issued twice.
fn mint(tx: &Connection) -> Result<i64, StoreError> {
    tx.query_row(
        "UPDATE approval_mark SET last = last + 1
         WHERE id = 1 AND last < 9223372036854775807 RETURNING last",
        [],
        |row| row.get(0),
    )
    .optional()?
    .ok_or(StoreError::Issue)
}

fn stage_json(stage: &Stage) -> Result<String, StoreError> {
    serde_json::to_string(stage).map_err(|_| StoreError::Issue)
}

/// The action while its thread is live.
fn load(conn: &Connection, id: i64) -> Result<Option<Action>, StoreError> {
    conn.query_row(
        "SELECT a.id, a.thread, a.version, a.stage, a.repo, a.account_id, a.account_login, a.title,
         a.body, a.approval, a.consumed, a.uncertain
         FROM actions a JOIN threads t ON t.id = a.thread AND t.state = 'live' WHERE a.id = ?1",
        params![id],
        |row| Ok(decode(row)),
    )
    .optional()?
    .transpose()
}

/// A row as an action; reads recheck the account id, since SQLite doesn't recheck stored rows.
fn decode(row: &Row<'_>) -> Result<Action, StoreError> {
    let bad = |_| StoreError::Issue;
    let positive = |column| -> Result<u64, StoreError> {
        u64::try_from(row.get::<_, i64>(column)?).map_err(|_| StoreError::Issue)
    };
    let revision = |column| -> Result<Option<ApprovalRev>, StoreError> {
        row.get::<_, Option<i64>>(column)?
            .map(|rev| u64::try_from(rev).map(ApprovalRev))
            .transpose()
            .map_err(|_| StoreError::Issue)
    };
    let account = match (
        row.get::<_, Option<String>>(5)?,
        row.get::<_, Option<String>>(6)?,
    ) {
        (Some(id), Some(login)) => Some(GitHubUser {
            id: id
                .parse::<u64>()
                .ok()
                .filter(|parsed| *parsed != 0 && parsed.to_string() == id)
                .ok_or(StoreError::Issue)?,
            login: GitHubLogin::try_from(login).map_err(bad)?,
        }),
        _ => None,
    };
    Ok(Action {
        draft: DraftId(positive(0)?),
        thread: ThreadId(positive(1)?),
        version: StateVersion(positive(2)?),
        stage: serde_json::from_str(&row.get::<_, String>(3)?).map_err(|_| StoreError::Issue)?,
        repo: row
            .get::<_, Option<String>>(4)?
            .map(RepoName::try_from)
            .transpose()
            .map_err(bad)?,
        account,
        title: IssueTitle::try_from(row.get::<_, String>(7)?).map_err(bad)?,
        body: IssueBody::try_from(row.get::<_, String>(8)?).map_err(bad)?,
        approval: revision(9)?,
        consumed: revision(10)?,
        uncertain: row.get(11)?,
    })
}
