//! Issue actions' rows (docs/development/issues.md): one transaction per change, live threads
//! only, and a fresh approval revision with every stage that offers a decision.

use kumochi_protocol::message::{
    ApprovalRev, ContentHash, Decision, DraftId, FailReason, GitHubLogin, IssueBody, IssueRefused,
    IssueTitle, RepoName, StateVersion, ThreadId,
};
use rusqlite::{Connection, OptionalExtension as _, Row, Transaction, params};

use super::{Store, StoreError, now_ms, sql_id};
use crate::connections::github::{api::GitHubUser, issues::CreateOutcome};
use crate::issues::{Action, Answer, Live, Lookup, Mark, Stage, Write, WriteScope};

impl Store {
    /// Executes one operation on the existing ordered store writer.
    ///
    /// # Errors
    /// Returns a content-free store failure with no partial transaction acknowledged.
    pub fn issue_write(
        &mut self,
        write: &crate::issues::Write,
        scope: Option<&WriteScope>,
    ) -> Result<Answer, StoreError> {
        let busy: u64 = self
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
            .map_err(quiet)?
            .try_into()
            .map_err(|_| StoreError::Issue)?;
        self.conn
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(quiet)?;
        let installed = if let Some(scope) = scope {
            let scope = scope.clone();
            self.conn
                .progress_handler(1000, Some(move || scope.stopped()))
        } else {
            Ok(())
        };
        self.issue_scope = scope.cloned();
        let reply = installed
            .map_err(quiet)
            .and_then(|()| self.apply_issue_write(write));
        self.issue_scope = None;
        let removed = if scope.is_some() {
            self.conn.progress_handler(0, None::<fn() -> bool>)
        } else {
            Ok(())
        };
        let restored = self
            .conn
            .busy_timeout(std::time::Duration::from_millis(busy));
        if removed.is_err() || restored.is_err() {
            self.serving = false;
            tracing::warn!("the issue SQL scope could not be restored; the writer must reopen");
        }
        // A committed answer stays durable even if cancellation arrives during COMMIT.
        reply
    }

    fn apply_issue_write(&mut self, write: &crate::issues::Write) -> Result<Answer, StoreError> {
        if self.issue_scope.as_ref().is_some_and(WriteScope::stopped) {
            return Err(StoreError::Issue);
        }
        match write {
            Write::Read(draft) => Ok(self.action(*draft)?.map_or(Answer::Gone, Answer::Unchanged)),
            Write::Rearm { draft, at, hash } => {
                let Ok(id) = i64::try_from(draft.0) else {
                    return Ok(Answer::Gone);
                };
                self.action_tx(|tx| {
                    let Some(action) = load(tx, id)? else {
                        return Ok(Answer::Gone);
                    };
                    if action.version != *at {
                        return Ok(Answer::Refused(IssueRefused::StaleApproval, action));
                    }
                    if action.hash() != *hash {
                        return Ok(Answer::Refused(IssueRefused::ContentChanged, action));
                    }
                    if !matches!(action.stage, Stage::Draft | Stage::Failed { .. }) {
                        return Ok(Answer::Refused(IssueRefused::NotAllowedNow, action));
                    }
                    commit_stage(tx, id, &action.stage, action.uncertain)?;
                    Ok(Answer::Changed(load(tx, id)?.ok_or(StoreError::Issue)?))
                })
            }
            Write::Consent {
                draft,
                at,
                hash,
                approval,
                decision,
                live,
            } => {
                let Ok(id) = i64::try_from(draft.0) else {
                    return Ok(Answer::Gone);
                };
                self.action_tx(|tx| {
                    consent_row(tx, id, Some(*at), hash, *approval, *decision, &live.live())
                })
            }
            Write::CancelledConsent {
                draft,
                at,
                hash,
                approval,
                live,
            } => {
                let Ok(id) = i64::try_from(draft.0) else {
                    return Ok(Answer::Gone);
                };
                self.action_tx(|tx| {
                    match consent_row(
                        tx,
                        id,
                        Some(*at),
                        hash,
                        *approval,
                        Decision::File,
                        &live.live(),
                    )? {
                        Answer::Changed(action)
                            if matches!(action.stage, Stage::Preflight { .. }) =>
                        {
                            change_row(tx, id, |current| {
                                Some(current.not_sent(action.version, FailReason::Interrupted))
                            })
                        }
                        answer => Ok(answer),
                    }
                })
            }
            Write::Preflighted { draft, at, result } => {
                self.preflighted(*draft, *at, result.clone())
            }
            Write::Posted { draft, at, outcome } => self.posted(*draft, *at, outcome),
            Write::Interrupted { draft, at } => self.interrupted(*draft, *at),
            Write::NotSent { draft, at, reason } => {
                self.change(*draft, |action| Some(action.not_sent(*at, *reason)))
            }
            Write::LookedUp {
                draft,
                episode,
                lookup,
            } => self.looked_up(*draft, *episode, *lookup),
            Write::Discard(draft) => self.discard_draft(*draft),
        }
    }

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
        self.action_tx(|tx| insert_draft(tx, thread, title, body, repo, account))
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
        self.change(draft, |action| Some(action.edit(title, body, repo)))
    }

    /// Applies the user's decision, checked against `live` (docs/development/issues.md#consent).
    /// File cannot bypass an exact unresolved attempt in another local draft.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn consent(
        &mut self,
        draft: DraftId,
        hash: &ContentHash,
        approval: ApprovalRev,
        decision: Decision,
        live: &Live<'_>,
    ) -> Result<Answer, StoreError> {
        let Ok(id) = i64::try_from(draft.0) else {
            return Ok(Answer::Gone);
        };
        self.action_tx(|tx| consent_row(tx, id, None, hash, approval, decision, live))
    }

    /// Binds the account, and the repository where none is chosen, to every Draft and Failed
    /// action in every live thread. Returns the actions it changed; none is filed.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when a read or write fails, with nothing written.
    pub fn bind_drafts(
        &mut self,
        account: Option<&GitHubUser>,
        repo: Option<&RepoName>,
    ) -> Result<Vec<Action>, StoreError> {
        self.action_tx(|tx| {
            let ids: Vec<i64> = tx
                .prepare("SELECT id FROM actions ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            let mut changed = Vec::new();
            for id in ids {
                if let Answer::Changed(action) =
                    change_row(tx, id, |action| Some(action.bind(account, repo)))?
                {
                    changed.push(action);
                }
            }
            Ok(changed)
        })
    }

    /// Ends the Preflight dispatched at `at`, the version of the answer that entered it.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn preflighted(
        &mut self,
        draft: DraftId,
        at: StateVersion,
        result: Result<Mark, FailReason>,
    ) -> Result<Answer, StoreError> {
        self.change(draft, |action| Some(action.preflighted(at, result)))
    }

    /// Records the create dispatched at `at`, the version of the answer that entered Posting.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails or the uncertain count is exhausted, with
    /// nothing written.
    pub fn posted(
        &mut self,
        draft: DraftId,
        at: StateVersion,
        outcome: &CreateOutcome,
    ) -> Result<Answer, StoreError> {
        self.change(draft, |action| action.posted(at, outcome))
    }

    /// Ends a send dispatched at `at` that the runtime gave up on. Whichever of this and the
    /// send's own completion commits first wins.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails or the uncertain count is exhausted, with
    /// nothing written.
    pub fn interrupted(&mut self, draft: DraftId, at: StateVersion) -> Result<Answer, StoreError> {
        self.change(draft, |action| action.interrupted(at))
    }

    /// Applies a lookup run while the action's `uncertain` count was `episode`.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn looked_up(
        &mut self,
        draft: DraftId,
        episode: u32,
        lookup: Lookup,
    ) -> Result<Answer, StoreError> {
        self.change(draft, |action| Some(action.looked_up(episode, lookup)))
    }

    /// Dismisses an action that isn't sending or settled.
    ///
    /// # Errors
    /// [`StoreError::Issue`] when the write fails, with nothing written.
    pub fn discard_draft(&mut self, draft: DraftId) -> Result<Answer, StoreError> {
        self.change(draft, |action| Some(action.discard()))
    }

    /// Ends the sends a crash cut short in live threads, as [`Stage::interrupted`] says, reading
    /// only each row's stage. Returns how many failed and how many became Unknown.
    pub(super) fn recover_sends(&mut self) -> Result<(usize, usize), StoreError> {
        self.action_tx(|tx| {
            let rows: Vec<(i64, String, u32)> = tx
                .prepare(
                    "SELECT a.id, a.stage, a.uncertain FROM actions a
                     JOIN threads t ON t.id = a.thread AND t.state = 'live'
                     WHERE json_extract(a.stage, '$.kind') IN ('preflight', 'posting') ORDER BY a.id",
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

    /// Applies one transition to the row reloaded in its own transaction.
    fn change(
        &mut self,
        draft: DraftId,
        apply: impl FnOnce(&mut Action) -> Option<Result<(), IssueRefused>>,
    ) -> Result<Answer, StoreError> {
        let Ok(id) = i64::try_from(draft.0) else {
            return Ok(Answer::Gone);
        };
        self.action_tx(|tx| change_row(tx, id, apply))
    }

    /// Runs `op` in one transaction, committed only when it succeeds.
    fn action_tx<T>(
        &mut self,
        op: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if self.issue_scope.as_ref().is_some_and(WriteScope::stopped) {
            return Err(StoreError::Issue);
        }
        let tx = self.conn.transaction().map_err(quiet)?;
        let value = op(&tx).map_err(quiet)?;
        if self.issue_scope.as_ref().is_some_and(WriteScope::stopped) {
            return Err(StoreError::Issue);
        }
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

pub(super) fn insert_draft(
    tx: &Connection,
    thread: i64,
    title: &IssueTitle,
    body: &IssueBody,
    repo: Option<&RepoName>,
    account: Option<&GitHubUser>,
) -> Result<Action, StoreError> {
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
    let id = tx.last_insert_rowid();
    event(tx, id, "draft")?;
    load(tx, id)?.ok_or(StoreError::Issue)
}

fn event(tx: &Connection, id: i64, kind: &str) -> Result<(), StoreError> {
    let inserted = tx.execute(
        "INSERT INTO events(thread, run, kind, text, body, created_ms)
         SELECT thread, NULL, ?2, title, json_object('draft', id), ?3 FROM actions WHERE id = ?1",
        params![id, kind, now_ms()],
    )?;
    if inserted == 1 {
        Ok(())
    } else {
        Err(StoreError::Issue)
    }
}

fn consent_row(
    tx: &Connection,
    id: i64,
    at: Option<StateVersion>,
    hash: &ContentHash,
    approval: ApprovalRev,
    decision: Decision,
    live: &Live<'_>,
) -> Result<Answer, StoreError> {
    change_row(tx, id, |action| {
        if at.is_some_and(|at| action.version != at) {
            return Some(Err(IssueRefused::StaleApproval));
        }
        let before = action.stage.clone();
        let consent = action.consent(hash, approval, decision, live);
        if decision != Decision::File || consent.is_err() || action.stage == before {
            return Some(consent);
        }
        let account = action.account.as_ref()?;
        let repo = action.repo.as_ref()?;
        // shortcut: only repository names are retained; store numeric IDs to recognize full renames.
        let other: Option<i64> = tx
            .query_row(
                "SELECT a.id FROM actions a
                     WHERE a.id <> ?1
                     AND a.account_id = ?2 AND a.repo = ?3 COLLATE NOCASE AND a.title = ?4 AND a.body = ?5
                     AND (json_extract(a.stage, '$.kind') IN ('preflight', 'posting', 'unknown')
                          OR (json_extract(a.stage, '$.kind') = 'discarded' AND a.uncertain > 0))
                     ORDER BY a.id LIMIT 1",
                params![
                    id,
                    account.id.to_string(),
                    repo.as_str(),
                    action.title.as_str(),
                    action.body.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .ok()?;
        if let Some(other) = other {
            let draft = u64::try_from(other).ok().filter(|id| *id != 0)?;
            action.stage = before;
            return Some(Err(IssueRefused::UnresolvedDraft {
                draft: DraftId(draft),
            }));
        }
        Some(consent)
    })
}

/// Applies a transition to the live row `id` and writes only a change. A refusal that used up the
/// current revision is written with a fresh one; `None` from `apply` fails the op.
fn change_row(
    tx: &Connection,
    id: i64,
    apply: impl FnOnce(&mut Action) -> Option<Result<(), IssueRefused>>,
) -> Result<Answer, StoreError> {
    let Some(before) = load(tx, id)? else {
        return Ok(Answer::Gone);
    };
    let mut after = before.clone();
    let refused = apply(&mut after).ok_or(StoreError::Issue)?.err();
    if after == before {
        return Ok(match refused {
            Some(why) => Answer::Refused(why, before),
            None => Answer::Unchanged(before),
        });
    }
    tx.execute(
        "UPDATE actions SET repo = ?2, account_id = ?3, account_login = ?4, title = ?5, body = ?6,
         consumed = ?7 WHERE id = ?1",
        params![
            id,
            after.repo.as_ref().map(RepoName::as_str),
            after.account.as_ref().map(|user| user.id.to_string()),
            after.account.as_ref().map(|user| user.login.as_str()),
            after.title.as_str(),
            after.body.as_str(),
            after
                .consumed
                .map(|rev| sql_id(rev.0, StoreError::Issue))
                .transpose()?
        ],
    )?;
    commit_stage(tx, id, &after.stage, after.uncertain)?;
    let action = load(tx, id)?.ok_or(StoreError::Issue)?;
    Ok(match refused {
        Some(why) => Answer::Refused(why, action),
        None => Answer::Changed(action),
    })
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
        event(tx, id, "action")
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
pub(super) fn load(conn: &Connection, id: i64) -> Result<Option<Action>, StoreError> {
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

/// A row as an action. Reads recheck the account and the stage's binding, since SQLite doesn't
/// recheck stored rows, so every action that loads can render.
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
        (None, None) => None,
        _ => return Err(StoreError::Issue),
    };
    let action = Action {
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
    };
    let bound = action.repo.is_some() && action.account.is_some();
    if bound
        || matches!(
            action.stage,
            Stage::Draft | Stage::Failed { .. } | Stage::Discarded
        )
    {
        Ok(action)
    } else {
        Err(StoreError::Issue)
    }
}
