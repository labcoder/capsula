//! Issue actions' rows: the schema's checks, create, read, edit and discard, recovery of cut-short
//! sends, hidden and deleted threads, purges, and what Debug prints.

use std::num::NonZeroU64;
use std::path::Path;

use kumochi_protocol::message::{
    ActionState, ApprovalRev, ContentHash, Decision, Deleted, DraftId, FailReason, GitHubLogin,
    IssueBody, IssueRefused, IssueTitle, IssueUrl, LookupResult, RepoName, RunId, StateVersion,
    ThreadId,
};
use rusqlite::{Connection, ErrorCode, params};

use super::tests::{purge, raw_bytes};
use super::{Recovered, RunEnd, Store, StoreError, Swap, schema};
use crate::connections::github::api::GitHubUser;
use crate::connections::github::issues::CreateOutcome;
use crate::connections::http_client::ResponseMeta;
use crate::issues::{Action, Answer, Live, Lookup, Mark, Stage};
use crate::test_support::Scratch;

pub(super) const A: &str = r#"{"at_s":100,"presend":[3]}"#;
pub(super) const B: &str = r#"{"at_s":200,"presend":[3,9]}"#;
pub(super) const C: &str = r#"{"at_s":300,"presend":[]}"#;

pub(super) fn opened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
}

/// A new live thread whose only run has ended.
pub(super) fn thread(store: &mut Store) -> ThreadId {
    let (thread, run) = store.high_water().expect("high water");
    let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(thread, Some("t"), run, "ask")
        .expect("turn");
    store
        .end_run(thread, run, &[], RunEnd::Done)
        .expect("ended");
    thread
}

pub(super) fn user(id: u64) -> GitHubUser {
    GitHubUser {
        id,
        login: GitHubLogin::new("octo").expect("login"),
    }
}

pub(super) fn repo() -> RepoName {
    RepoName::new("octo/repo").expect("repo")
}

pub(super) fn title(text: &str) -> IssueTitle {
    IssueTitle::new(text).expect("title")
}

pub(super) fn body(text: &str) -> IssueBody {
    IssueBody::new(text).expect("body")
}

pub(super) fn mark(json: &str) -> Mark {
    serde_json::from_str(json).expect("mark")
}

pub(super) fn issue(number: u64) -> NonZeroU64 {
    NonZeroU64::new(number).expect("an issue number")
}

pub(super) fn uncertain() -> CreateOutcome {
    CreateOutcome::Uncertain {
        status: None,
        meta: ResponseMeta::default(),
    }
}

fn sql(id: u64) -> i64 {
    i64::try_from(id).expect("small")
}

pub(super) fn draft(store: &mut Store, thread: ThreadId) -> Action {
    store
        .create_draft(
            thread,
            &title("T"),
            &body("B"),
            Some(&repo()),
            Some(&user(7)),
        )
        .expect("draft")
}

/// GitHub's ids for `repo()` and the other repository the listings name.
pub(super) const ID: NonZeroU64 = NonZeroU64::new(7).expect("nonzero");
pub(super) const OTHER: NonZeroU64 = NonZeroU64::new(8).expect("nonzero");

/// The action with `repo()` bound to `ID`, as its review over a listing leaves it, without that
/// review's new revision.
pub(super) fn bind(store: &mut Store, action: &Action) -> Action {
    let id = i64::try_from(action.draft.0).expect("a stored draft");
    store
        .conn
        .execute("UPDATE actions SET repo_id = 7 WHERE id = ?1", [id])
        .expect("bind");
    store.action(action.draft).expect("read").expect("live")
}

/// A new draft, reviewed: see [`bind`].
pub(super) fn reviewed(store: &mut Store, thread: ThreadId) -> Action {
    let action = draft(store, thread);
    bind(store, &action)
}

pub(super) fn posting(mark: &str) -> String {
    format!(r#"{{"kind":"posting","mark":{mark},"unresolved":null}}"#)
}

pub(super) fn unknown(lookup: &str) -> String {
    format!(r#"{{"kind":"unknown","mark":{A},"lookup":{lookup}}}"#)
}

/// Puts a row in a stage that only a send reaches; no product API before admission can.
pub(super) fn seed(store: &Store, draft: DraftId, stage: &str, uncertain: u32) {
    store
        .conn
        .execute(
            "UPDATE actions SET stage = ?2, uncertain = ?3, approval = NULL WHERE id = ?1",
            params![sql(draft.0), stage, uncertain],
        )
        .expect("seeded");
}

/// Every column of every row, and the approval mark, to show what an op left alone.
pub(super) fn snapshot(conn: &Connection) -> (Vec<String>, i64) {
    let rows = conn
        .prepare(
            "SELECT json_array(id, thread, version, stage, repo, account_id, account_login, title,
             body, approval, consumed, uncertain, created_ms) FROM actions ORDER BY id",
        )
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    let last = conn
        .query_row("SELECT last FROM approval_mark WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("mark");
    (rows, last)
}

fn constraint(result: &rusqlite::Result<usize>) -> bool {
    matches!(result, Err(rusqlite::Error::SqliteFailure(error, _))
        if error.code == ErrorCode::ConstraintViolation)
}

fn issue_events(store: &Store) -> Vec<(String, String, String)> {
    store
        .conn
        .prepare("SELECT kind, text, body FROM events WHERE kind IN ('draft','action') ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn issue_events_share_action_and_approval_commits_but_not_noops_or_replays() {
    let scratch = Scratch::new("issue-event-transactions");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let created = store
        .create_draft(owner, &title("title"), &body("private body"), None, None)
        .unwrap();
    assert_eq!(
        issue_events(&store),
        [("draft".into(), "title".into(), "{\"draft\":1}".into())]
    );
    assert!(matches!(
        store.edit_draft(created.draft, &title("title"), &body("private body"), None),
        Ok(Answer::Unchanged(_))
    ));
    assert_eq!(issue_events(&store).len(), 1);
    let disconnected = Live {
        account: None,
        capacity: true,
        repos: &[],
        repo_ids: &[],
    };
    assert!(matches!(
        store.consent(
            created.draft,
            &created.hash(),
            created.approval.unwrap(),
            Decision::File,
            &disconnected
        ),
        Ok(Answer::Refused(IssueRefused::NotConnected, _))
    ));
    assert_eq!(
        issue_events(&store),
        [
            ("draft".into(), "title".into(), "{\"draft\":1}".into()),
            ("action".into(), "title".into(), "{\"draft\":1}".into())
        ]
    );
    assert!(matches!(
        store.consent(
            created.draft,
            &created.hash(),
            created.approval.unwrap(),
            Decision::File,
            &disconnected
        ),
        Ok(Answer::Unchanged(_))
    ));
    assert!(matches!(store.discard_draft(DraftId(99)), Ok(Answer::Gone)));
    assert_eq!(issue_events(&store).len(), 2);
    let before = snapshot(&store.conn);
    let events = issue_events(&store);
    store.conn.execute_batch("CREATE TEMP TRIGGER reject_issue_event BEFORE INSERT ON events WHEN new.kind IN ('draft','action') BEGIN SELECT RAISE(ABORT,'event failure'); END;").unwrap();
    assert!(matches!(
        store.edit_draft(created.draft, &title("changed"), &body("other"), None),
        Err(StoreError::Issue)
    ));
    assert!(matches!(
        store.create_draft(owner, &title("new"), &body("body"), None, None),
        Err(StoreError::Issue)
    ));
    assert_eq!(snapshot(&store.conn), before);
    assert_eq!(issue_events(&store), events);
}

#[test]
fn recovery_events_copy_titles_in_sql_and_rollback_the_entire_send_batch() {
    let scratch = Scratch::new("issue-recovery-events");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let first = draft(&mut store, owner).draft;
    let second = draft(&mut store, owner).draft;
    seed(&store, first, &posting(A), 0);
    seed(&store, second, &posting(A), 0);
    store
        .conn
        .execute(
            "UPDATE actions SET body = 'invalid\rbody' WHERE id = ?1",
            [sql(first.0)],
        )
        .unwrap();
    let before = snapshot(&store.conn);
    let events = issue_events(&store);
    store.conn.execute_batch("CREATE TEMP TRIGGER reject_second_event BEFORE INSERT ON events WHEN new.kind = 'action' AND json_extract(new.body,'$.draft') = 2 BEGIN SELECT RAISE(ABORT,'second event'); END;").unwrap();
    assert!(matches!(store.recover(), Err(StoreError::Issue)));
    assert_eq!(snapshot(&store.conn), before);
    assert_eq!(issue_events(&store), events);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_second_event")
        .unwrap();
    assert_eq!(store.recover().unwrap().unknown_sends, 2);
    assert_eq!(
        issue_events(&store)
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>(),
        ["draft", "draft", "action", "action"]
    );
    assert!(matches!(store.action(first), Err(StoreError::Issue)));
    assert_eq!(store.recover().unwrap().unknown_sends, 0);
    assert_eq!(issue_events(&store).len(), 4);
}

#[test]
fn version_six_upgrades_with_its_rows_and_an_unused_mark() {
    let scratch = Scratch::new("actions-v6");
    std::fs::create_dir_all(scratch.dir()).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    schema::migrate_to(&mut conn, 6).expect("V6");
    conn.execute(
        "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1)",
        [],
    )
    .expect("an older row");
    drop(conn);
    let mut store = opened(&scratch);
    let version: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 11);
    let marks: Vec<(i64, i64)> = store
        .conn
        .prepare("SELECT id, last FROM approval_mark")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(marks, [(1, 0)]);
    assert_eq!(snapshot(&store.conn), (Vec::new(), 0));
    let kept: String = store
        .conn
        .query_row("SELECT title FROM threads WHERE id = 5", [], |row| {
            row.get(0)
        })
        .expect("kept");
    assert_eq!(kept, "kept");
    assert_eq!(
        draft(&mut store, ThreadId(5)).approval,
        Some(ApprovalRev(1))
    );
}

#[test]
fn a_draft_is_created_read_edited_and_discarded() {
    let scratch = Scratch::new("actions-life");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let expected = Action {
        draft: DraftId(1),
        thread,
        version: StateVersion(1),
        stage: Stage::Draft,
        repo: Some(repo()),
        account: Some(user(7)),
        title: title("T"),
        body: body("B"),
        approval: Some(ApprovalRev(1)),
        consumed: None,
        uncertain: 0,
        repo_id: None,
    };
    assert_eq!(draft(&mut store, thread), expected);
    assert_eq!(
        store.action(DraftId(1)).expect("read"),
        Some(expected.clone())
    );
    assert_eq!(expected.state(), ActionState::Draft);
    let same = store.edit_draft(DraftId(1), &title("T"), &body("B"), Some(&repo()));
    assert!(matches!(same, Ok(Answer::Unchanged(action)) if action == expected));
    let Ok(Answer::Changed(edited)) = store.edit_draft(DraftId(1), &title("T2"), &body("B2"), None)
    else {
        panic!("edited");
    };
    let edited_expected = Action {
        version: StateVersion(2),
        repo: None,
        title: title("T2"),
        body: body("B2"),
        approval: Some(ApprovalRev(2)),
        ..expected
    };
    assert_eq!(edited, edited_expected);
    let Ok(Answer::Changed(discarded)) = store.discard_draft(DraftId(1)) else {
        panic!("discarded");
    };
    let discarded_expected = Action {
        version: StateVersion(3),
        stage: Stage::Discarded,
        approval: None,
        ..edited_expected
    };
    assert_eq!(discarded, discarded_expected);
    assert_eq!(
        discarded.state(),
        ActionState::Discarded { maybe_filed: false }
    );
    assert_eq!(store.action(DraftId(1)).expect("read"), Some(discarded));
    let past = store.edit_draft(DraftId(u64::MAX), &title("T"), &body("B"), None);
    assert!(matches!(past, Ok(Answer::Gone)));
    assert!(matches!(store.discard_draft(DraftId(2)), Ok(Answer::Gone)));
    assert_eq!(store.action(DraftId(u64::MAX)).expect("read"), None);
    let orphan = store.create_draft(ThreadId(99), &title("T"), &body("B"), None, None);
    assert!(matches!(orphan, Err(StoreError::NoThread)));
    assert_eq!(
        snapshot(&store.conn).1,
        2,
        "a refused create issues no revision"
    );
}

/// Each stage with what edit and discard turn it into; `None` is a refusal that writes nothing.
#[test]
fn edit_and_discard_take_only_their_stages() {
    let scratch = Scratch::new("actions-stages");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let cases = [
        (r#"{"kind":"draft"}"#.to_owned(), 0, true, Some(false)),
        (
            r#"{"kind":"failed","reason":"Network"}"#.to_owned(),
            0,
            true,
            Some(false),
        ),
        (unknown(r#""Pending""#), 1, false, Some(true)),
        (
            r#"{"kind":"preflight","unresolved":null}"#.to_owned(),
            0,
            false,
            None,
        ),
        (posting(A), 0, false, None),
        (
            r#"{"kind":"filed","number":7,"confirmed":false}"#.to_owned(),
            0,
            false,
            None,
        ),
        (r#"{"kind":"discarded"}"#.to_owned(), 1, false, None),
    ];
    for (stage, uncertain, editable, discard) in cases {
        let id = draft(&mut store, thread).draft;
        seed(&store, id, &stage, uncertain);
        let before = snapshot(&store.conn);
        match store.edit_draft(id, &title("E"), &body("E"), None) {
            Ok(Answer::Changed(action)) if editable => {
                assert_eq!((action.stage, action.title), (Stage::Draft, title("E")));
            }
            Ok(Answer::Refused(IssueRefused::NotAllowedNow, _)) if !editable => {
                assert_eq!(snapshot(&store.conn), before, "{stage}");
            }
            other => panic!("{stage}: edit answered {other:?}"),
        }
        seed(&store, id, &stage, uncertain);
        let before = snapshot(&store.conn);
        match (store.discard_draft(id), discard) {
            (Ok(Answer::Changed(action)), Some(maybe_filed)) => assert_eq!(
                (action.state(), action.stage),
                (ActionState::Discarded { maybe_filed }, Stage::Discarded)
            ),
            (Ok(Answer::Refused(IssueRefused::NotAllowedNow, _)), None) => {
                assert_eq!(snapshot(&store.conn), before, "{stage}");
            }
            (other, _) => panic!("{stage}: discard answered {other:?}"),
        }
    }
}

#[test]
fn recovery_ends_interrupted_sends_and_keeps_the_earliest_mark() {
    let scratch = Scratch::new("actions-recover");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let ids: Vec<DraftId> = (0..5).map(|_| draft(&mut store, thread).draft).collect();
    seed(
        &store,
        ids[0],
        r#"{"kind":"preflight","unresolved":null}"#,
        0,
    );
    seed(&store, ids[1], &posting(A), 0);
    let preflight = format!(r#"{{"kind":"preflight","unresolved":{A}}}"#);
    seed(&store, ids[2], &preflight, 1);
    let again = format!(r#"{{"kind":"posting","mark":{B},"unresolved":{A}}}"#);
    seed(&store, ids[3], &again, 1);
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("reopen");
    let recovered = Recovered {
        failed_sends: 1,
        unknown_sends: 3,
        ..Recovered::default()
    };
    let result = store.recover();
    assert!(
        matches!(&result, Ok(done) if *done == recovered),
        "{result:?}"
    );
    let pending = |uncertain, maybe_duplicate| {
        (
            Stage::Unknown {
                mark: mark(A),
                lookup: LookupResult::Pending,
            },
            uncertain,
            ActionState::Unknown {
                lookup: LookupResult::Pending,
                maybe_duplicate,
            },
        )
    };
    let expected = [
        (
            Stage::Failed {
                reason: FailReason::Interrupted,
            },
            0,
            ActionState::Failed(FailReason::Interrupted),
        ),
        pending(1, false),
        pending(1, false),
        pending(2, true),
    ];
    let mut approvals = Vec::new();
    for (id, (stage, uncertain, state)) in ids.iter().zip(expected) {
        let action = store.action(*id).expect("read").expect("live");
        assert_eq!(
            (
                &action.stage,
                action.uncertain,
                action.state(),
                action.version
            ),
            (&stage, uncertain, state, StateVersion(2))
        );
        approvals.extend(action.approval.map(|rev| rev.0));
    }
    approvals.sort_unstable();
    assert_eq!(approvals, [6, 7, 8, 9]);
    let untouched = store.action(ids[4]).expect("read").expect("live");
    assert_eq!(
        (untouched.stage, untouched.version),
        (Stage::Draft, StateVersion(1))
    );
    assert_eq!(store.recover().expect("again"), Recovered::default());
}

#[test]
fn hidden_threads_take_no_action_input_and_nothing_comes_back() {
    let scratch = Scratch::new("actions-hidden");
    let mut store = opened(&scratch);
    let live = thread(&mut store);
    let kept = draft(&mut store, live);
    let hidden = thread(&mut store);
    let unbound = store
        .create_draft(hidden, &title("T"), &body("B"), None, None)
        .expect("draft");
    let drafts = [0, 1, 2].map(|_| draft(&mut store, hidden).draft);
    let ids = [unbound.draft, drafts[0], drafts[1], drafts[2]];
    seed(&store, ids[1], &posting(A), 0);
    seed(&store, ids[2], &unknown(r#""Pending""#), 1);
    seed(
        &store,
        ids[3],
        r#"{"kind":"preflight","unresolved":null}"#,
        0,
    );
    let (seven, listed) = (user(7), [repo()]);
    let live = Live {
        account: Some(&seven),
        capacity: true,
        repos: &listed,
        repo_ids: &[],
    };
    store.begin_delete(hidden).expect("hidden");
    store
        .conn
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER keep_actions BEFORE DELETE ON actions WHEN old.thread = {}
             BEGIN SELECT RAISE(ABORT, 'kept'); END;",
            hidden.0
        ))
        .expect("removal stuck");
    let before = snapshot(&store.conn);
    let refused = |store: &mut Store| {
        let created = store.create_draft(hidden, &title("N"), &body("N"), None, None);
        assert!(matches!(created, Err(StoreError::NoThread)));
        for id in ids {
            assert_eq!(store.action(id).expect("read"), None);
            let edited = store.edit_draft(id, &title("E"), &body("E"), None);
            assert!(matches!(edited, Ok(Answer::Gone)));
            assert!(matches!(store.discard_draft(id), Ok(Answer::Gone)));
            let at = StateVersion(1);
            let answers = [
                store.consent(id, &unbound.hash(), ApprovalRev(2), Decision::File, &live),
                store.preflighted(id, at, Ok(mark(A))),
                store.posted(id, at, &uncertain()),
                store.interrupted(id, at),
                store.looked_up(id, 1, Lookup::Placed(issue(7))),
            ];
            assert!(
                answers
                    .iter()
                    .all(|answer| matches!(answer, Ok(Answer::Gone))),
                "{answers:?}"
            );
        }
        let bound = store.bind_drafts(Some(&seven), Some(&repo()));
        assert_eq!(bound.expect("bind"), []);
    };
    refused(&mut store);
    let stuck = Recovered {
        failed_deletions: 1,
        ..Recovered::default()
    };
    assert_eq!(store.recover().expect("recover"), stuck);
    assert_eq!(snapshot(&store.conn), before, "no row or revision changed");
    store
        .conn
        .execute_batch("DROP TRIGGER temp.keep_actions;")
        .expect("unstuck");
    let finished = Recovered {
        finished_deletions: 1,
        purge_pending: true,
        ..Recovered::default()
    };
    assert_eq!(store.recover().expect("recover"), finished);
    refused(&mut store);
    let (rows, last) = snapshot(&store.conn);
    assert_eq!(
        (rows.len(), last),
        (1, before.1),
        "only the live draft is left"
    );
    assert_eq!(store.action(kept.draft).expect("read"), Some(kept));
}

#[test]
fn revisions_and_draft_ids_are_never_reused_after_deletion_and_purge() {
    let scratch = Scratch::new("actions-reuse");
    let mut store = opened(&scratch);
    let deleted = thread(&mut store);
    draft(&mut store, deleted);
    let second = draft(&mut store, deleted).draft;
    let edited = store.edit_draft(second, &title("E"), &body("E"), None);
    assert!(matches!(edited, Ok(Answer::Changed(_))));
    assert_eq!(store.delete(deleted).outcome, Deleted::Removed);
    assert_eq!(purge(&mut store), Swap::Done);
    drop(store);
    let mut store = opened(&scratch);
    let next = thread(&mut store);
    let fresh = draft(&mut store, next);
    assert_eq!(
        (fresh.draft, fresh.approval),
        (DraftId(3), Some(ApprovalRev(4)))
    );
    let (seven, listed) = (user(7), [repo()]);
    let live = Live {
        account: Some(&seven),
        capacity: true,
        repos: &listed,
        repo_ids: &[],
    };
    let gone = store.consent(second, &fresh.hash(), ApprovalRev(3), Decision::File, &live);
    assert!(matches!(gone, Ok(Answer::Gone)), "{gone:?}");
    let before = snapshot(&store.conn);
    let reused = store.consent(
        fresh.draft,
        &fresh.hash(),
        ApprovalRev(3),
        Decision::File,
        &live,
    );
    assert!(
        matches!(reused, Ok(Answer::Refused(IssueRefused::StaleApproval, _))),
        "{reused:?}"
    );
    assert_eq!(snapshot(&store.conn), before);
}

#[test]
fn deleted_issue_text_leaves_every_file() {
    const MARKERS: [&str; 3] = ["zqxissuetitle", "zqxissuebody", "zqxissueedited"];
    let found = |db: &Path| {
        let bytes = raw_bytes(db);
        MARKERS.map(|marker| {
            bytes
                .windows(marker.len())
                .any(|at| at == marker.as_bytes())
        })
    };
    let scratch = Scratch::new("actions-purge");
    let mut store = opened(&scratch);
    let live = thread(&mut store);
    let kept = draft(&mut store, live);
    let deleted = thread(&mut store);
    let headline = title("Title zqxissuetitle");
    let planted = store
        .create_draft(deleted, &headline, &body("Body zqxissuebody"), None, None)
        .expect("draft")
        .draft;
    let edited = store.edit_draft(planted, &headline, &body("Body zqxissueedited"), None);
    assert!(matches!(edited, Ok(Answer::Changed(_))));
    assert_eq!(
        found(&scratch.db()),
        [true; 3],
        "every marker is on disk first"
    );
    assert_eq!(store.delete(deleted).outcome, Deleted::Removed);
    assert_eq!(purge(&mut store), Swap::Done);
    assert_eq!(found(&scratch.db()), [false; 3], "found while open");
    drop(store);
    assert_eq!(found(&scratch.db()), [false; 3], "found after close");
    let store = opened(&scratch);
    assert_eq!(store.action(kept.draft).expect("read"), Some(kept));
}

#[test]
fn a_failure_after_the_mint_rolls_the_mint_back() {
    let scratch = Scratch::new("actions-rollback");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let editable = draft(&mut store, thread).draft;
    let sending = draft(&mut store, thread).draft;
    seed(&store, sending, &posting(A), 0);
    // Each fires only once its transaction's mint has raised the mark from 2 to 3.
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER after_mint_update BEFORE UPDATE ON actions
             WHEN (SELECT last FROM approval_mark) = 3 BEGIN SELECT RAISE(ABORT, 'trap'); END;
             CREATE TEMP TRIGGER after_mint_insert BEFORE INSERT ON actions
             WHEN (SELECT last FROM approval_mark) = 3 BEGIN SELECT RAISE(ABORT, 'trap'); END;",
        )
        .expect("traps");
    let before = snapshot(&store.conn);
    let edit = |store: &mut Store| store.edit_draft(editable, &title("E"), &body("E"), None);
    let create =
        |store: &mut Store| store.create_draft(thread, &title("N"), &body("N"), None, None);
    assert!(matches!(edit(&mut store), Err(StoreError::Issue)));
    assert!(matches!(create(&mut store), Err(StoreError::Issue)));
    assert!(matches!(store.recover(), Err(StoreError::Issue)));
    assert_eq!(
        snapshot(&store.conn),
        before,
        "each mint rolled back with its op"
    );
    store
        .conn
        .execute_batch("DROP TRIGGER temp.after_mint_update; DROP TRIGGER temp.after_mint_insert;")
        .expect("untrapped");
    let Ok(Answer::Changed(edited)) = edit(&mut store) else {
        panic!("edited");
    };
    assert_eq!(edited.approval, Some(ApprovalRev(3)));
    assert_eq!(store.recover().expect("recover").unknown_sends, 1);
    let recovered = store.action(sending).expect("read").expect("live");
    assert_eq!(recovered.approval, Some(ApprovalRev(4)));
    assert_eq!(
        create(&mut store).expect("draft").approval,
        Some(ApprovalRev(5))
    );
    let cut = draft(&mut store, thread).draft;
    seed(&store, cut, &posting(A), 0);
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER after_mint BEFORE UPDATE ON actions
             WHEN (SELECT last FROM approval_mark) = 7 BEGIN SELECT RAISE(ABORT, 'trap'); END;",
        )
        .expect("trap");
    let current = store.action(editable).expect("read").expect("live");
    let disconnected = Live {
        account: None,
        capacity: true,
        repos: &[],
        repo_ids: &[],
    };
    let refuse = |store: &mut Store| {
        store.consent(
            editable,
            &current.hash(),
            ApprovalRev(3),
            Decision::File,
            &disconnected,
        )
    };
    let before = snapshot(&store.conn);
    let interrupted = store.interrupted(cut, StateVersion(1));
    assert!(
        matches!(interrupted, Err(StoreError::Issue)),
        "{interrupted:?}"
    );
    assert!(matches!(refuse(&mut store), Err(StoreError::Issue)));
    assert_eq!(
        snapshot(&store.conn),
        before,
        "a used-up revision rolls back with its mint"
    );
    store
        .conn
        .execute_batch("DROP TRIGGER temp.after_mint;")
        .expect("untrapped");
    let Ok(Answer::Changed(interrupted)) = store.interrupted(cut, StateVersion(1)) else {
        panic!("interrupted");
    };
    assert_eq!(interrupted.approval, Some(ApprovalRev(7)));
    let Ok(Answer::Refused(IssueRefused::NotConnected, refused)) = refuse(&mut store) else {
        panic!("refused");
    };
    assert_eq!(
        (refused.consumed, refused.approval),
        (Some(ApprovalRev(3)), Some(ApprovalRev(8)))
    );
}

#[test]
fn the_schema_refuses_bad_stage_kinds_and_uncertain_drafts() {
    let scratch = Scratch::new("actions-checks");
    let mut store = opened(&scratch);
    let thread = sql(thread(&mut store).0);
    let insert = |stage: &str, uncertain: u32| {
        store.conn.execute(
            "INSERT INTO actions (thread, version, stage, repo, account_id, account_login, title,
             body, uncertain, created_ms) VALUES (?1, 1, ?2, 'octo/repo', '7', 'octo', 'T', 'B', ?3, 0)",
            params![thread, stage, uncertain],
        )
    };
    for stage in [
        "{}",
        r#"{"kind":null}"#,
        "[]",
        "null",
        r#"{"kind":"sending"}"#,
        r#"{"kind":7}"#,
    ] {
        assert!(constraint(&insert(stage, 0)), "{stage}");
    }
    assert!(constraint(&insert(r#"{"kind":"draft"}"#, 1)));
    assert!(constraint(&insert(
        r#"{"kind":"failed","reason":"Network"}"#,
        1
    )));
    assert_eq!(insert(r#"{"kind":"draft"}"#, 0).expect("a draft"), 1);
    assert_eq!(insert(&unknown(r#""Pending""#), 1).expect("an unknown"), 1);
}

#[test]
fn account_ids_keep_every_u64_and_refuse_noncanonical_text() {
    let scratch = Scratch::new("actions-accounts");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    for (id, text) in [
        (1_u64 << 63, "9223372036854775808"),
        (u64::MAX, "18446744073709551615"),
    ] {
        let action = store
            .create_draft(
                thread,
                &title("T"),
                &body("B"),
                Some(&repo()),
                Some(&user(id)),
            )
            .expect("draft");
        let stored: String = store
            .conn
            .query_row(
                "SELECT account_id FROM actions WHERE id = ?1",
                [sql(action.draft.0)],
                |row| row.get(0),
            )
            .expect("stored");
        assert_eq!((action.account, stored.as_str()), (Some(user(id)), text));
    }
    let before = snapshot(&store.conn);
    let zero = store.create_draft(
        thread,
        &title("T"),
        &body("B"),
        Some(&repo()),
        Some(&user(0)),
    );
    assert!(matches!(zero, Err(StoreError::Issue)));
    assert_eq!(snapshot(&store.conn), before);
    let set = |text: &str| {
        store
            .conn
            .execute("UPDATE actions SET account_id = ?1 WHERE id = 1", [text])
    };
    for text in ["0", "007", "+5"] {
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("checks off");
        set(text).expect("planted past the check");
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("checks on");
        assert!(
            matches!(store.action(DraftId(1)), Err(StoreError::Issue)),
            "{text}"
        );
    }
    for half in [
        "UPDATE actions SET account_id = '7', account_login = NULL WHERE id = 1",
        "UPDATE actions SET account_id = NULL, account_login = 'octo' WHERE id = 1",
    ] {
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("checks off");
        store
            .conn
            .execute(half, [])
            .expect("planted past the check");
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("checks on");
        assert!(
            matches!(store.action(DraftId(1)), Err(StoreError::Issue)),
            "{half}"
        );
    }
    for text in ["0", "007", "+5", "1e3", "", "123456789012345678901"] {
        assert!(constraint(&set(text)), "{text}");
    }
    set("18446744073709551616").expect("twenty digits pass the check");
    assert!(matches!(store.action(DraftId(1)), Err(StoreError::Issue)));
}

#[test]
fn every_action_that_loads_can_render() {
    let scratch = Scratch::new("actions-bound");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let (owner, name) = ("o".repeat(39), "r".repeat(100));
    let longest = RepoName::new(&format!("{owner}/{name}")).expect("the longest repository");
    let widest = store
        .create_draft(
            thread,
            &title("T"),
            &body("B"),
            Some(&longest),
            Some(&user(7)),
        )
        .expect("draft")
        .draft;
    seed(
        &store,
        widest,
        r#"{"kind":"filed","number":18446744073709551615,"confirmed":true}"#,
        0,
    );
    let url = format!("https://github.com/{owner}/{name}/issues/18446744073709551615");
    assert_eq!(
        store.action(widest).expect("read").expect("live").state(),
        ActionState::Filed {
            number: u64::MAX,
            url: IssueUrl::new(&url).expect("a checked URL"),
            confirmed_by_user: true,
            maybe_duplicate: false
        }
    );
    let unbound = [
        (
            r#"{"kind":"filed","number":7,"confirmed":false}"#.to_owned(),
            "repo = NULL",
        ),
        (posting(A), "account_id = NULL, account_login = NULL"),
    ];
    let mut planted = Vec::new();
    store
        .conn
        .pragma_update(None, "ignore_check_constraints", true)
        .expect("checks off");
    for (stage, unbind) in &unbound {
        let id = draft(&mut store, thread).draft;
        seed(&store, id, stage, 0);
        store
            .conn
            .execute(
                &format!("UPDATE actions SET {unbind} WHERE id = ?1"),
                [sql(id.0)],
            )
            .expect("planted past the check");
        planted.push(id);
    }
    store
        .conn
        .pragma_update(None, "ignore_check_constraints", false)
        .expect("checks on");
    for (id, (stage, _)) in planted.into_iter().zip(&unbound) {
        assert!(
            matches!(store.action(id), Err(StoreError::Issue)),
            "{stage}"
        );
    }
}

#[test]
fn recovery_reads_only_stages_and_rolls_back_whole_on_a_corrupt_one() {
    let scratch = Scratch::new("actions-corrupt");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let stages = [
        (r#"{"kind":"preflight","unresolved":null}"#.to_owned(), 0),
        (posting(A), 0),
        (r#"{"kind":"draft"}"#.to_owned(), 0),
        (
            r#"{"kind":"filed","number":7,"confirmed":false}"#.to_owned(),
            0,
        ),
        (r#"{"kind":"failed","reason":"Network"}"#.to_owned(), 0),
        (unknown(r#""Pending""#), 1),
        (r#"{"kind":"discarded"}"#.to_owned(), 1),
        (
            r#"{"kind":"posting","mark":{"at_s":0,"presend":[]},"unresolved":null}"#.to_owned(),
            0,
        ),
    ];
    let ids: Vec<DraftId> = stages
        .iter()
        .map(|(stage, uncertain)| {
            let id = draft(&mut store, thread).draft;
            seed(&store, id, stage, *uncertain);
            id
        })
        .collect();
    store
        .conn
        .execute_batch(&format!(
            "UPDATE actions SET consumed = 1;
             UPDATE actions SET title = 'T' || char(7), body = 'B' || char(7) WHERE id = {};",
            ids[0].0
        ))
        .expect("planted");
    drop(store);
    let mut store = Store::open(&scratch.db()).expect("opening reads no action");
    let before = snapshot(&store.conn);
    for attempt in 0..2 {
        let recovered = store.recover();
        assert!(
            matches!(recovered, Err(StoreError::Issue)),
            "{attempt}: {recovered:?}"
        );
        assert_eq!(snapshot(&store.conn), before, "the whole batch rolled back");
    }
    seed(&store, ids[7], &posting(B), 0);
    let recovered = store.recover().expect("recover");
    assert_eq!((recovered.failed_sends, recovered.unknown_sends), (1, 2));
    let stored: Vec<(String, Option<i64>)> = store
        .conn
        .prepare("SELECT stage, consumed FROM actions ORDER BY id")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    let unknown_b = format!(r#"{{"kind":"unknown","mark":{B},"lookup":"Pending"}}"#);
    assert_eq!(
        [&stored[0], &stored[1], &stored[7]],
        [
            &(
                r#"{"kind":"failed","reason":"Interrupted"}"#.to_owned(),
                Some(1)
            ),
            &(unknown(r#""Pending""#), Some(1)),
            &(unknown_b, Some(1)),
        ]
    );
    assert_eq!(
        snapshot(&store.conn).0[2..7],
        before.0[2..7],
        "settled rows stay"
    );
    assert!(
        matches!(store.action(ids[0]), Err(StoreError::Issue)),
        "recovery never read the content that fails to load"
    );
}

#[test]
fn counters_fail_closed_at_their_limits() {
    use super::rearm_tests::{events, rearm};

    let scratch = Scratch::new("actions-limits");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let editable = draft(&mut store, owner).draft;
    let edit = |store: &mut Store| store.edit_draft(editable, &title("E"), &body("E"), None);
    store
        .conn
        .execute("UPDATE approval_mark SET last = 9223372036854775807", [])
        .expect("mark at its limit");
    let before = snapshot(&store.conn);
    let created = store.create_draft(owner, &title("N"), &body("N"), None, None);
    assert!(matches!(created, Err(StoreError::Issue)));
    assert!(matches!(edit(&mut store), Err(StoreError::Issue)));
    let disconnected = Live {
        account: None,
        capacity: true,
        repos: &[],
        repo_ids: &[],
    };
    let refused = store.consent(
        editable,
        &ContentHash([0; 32]),
        ApprovalRev(1),
        Decision::File,
        &disconnected,
    );
    assert!(matches!(refused, Err(StoreError::Issue)), "{refused:?}");
    assert_eq!(snapshot(&store.conn), before, "no revision repeats");
    store
        .conn
        .execute_batch(
            "UPDATE approval_mark SET last = 10;
             UPDATE actions SET version = 9223372036854775807 WHERE id = 1;",
        )
        .expect("version at its limit");
    let before = snapshot(&store.conn);
    assert!(matches!(edit(&mut store), Err(StoreError::Issue)));
    assert_eq!(snapshot(&store.conn), before, "the version holds");
    let sending = draft(&mut store, owner).draft;
    seed(&store, sending, &posting(A), u32::MAX);
    let before = snapshot(&store.conn);
    let posted = store.posted(sending, StateVersion(1), &uncertain());
    assert!(matches!(posted, Err(StoreError::Issue)), "{posted:?}");
    let interrupted = store.interrupted(sending, StateVersion(1));
    assert!(
        matches!(interrupted, Err(StoreError::Issue)),
        "{interrupted:?}"
    );
    assert!(matches!(store.recover(), Err(StoreError::Issue)));
    assert_eq!(snapshot(&store.conn), before, "the count holds");

    let max = 9_223_372_036_854_775_807;
    for (mark, version, expected_approval, expected_version) in
        [(max - 1, 1, max, 2), (1, max - 1, 2, max)]
    {
        let scratch = Scratch::new("issue-rearm-exhaustion");
        let mut store = opened(&scratch);
        let owner = thread(&mut store);
        let action = draft(&mut store, owner);
        store
            .conn
            .execute("UPDATE approval_mark SET last = ?1", [sql(mark)])
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE actions SET version = ?1 WHERE id = ?2",
                params![sql(version), sql(action.draft.0)],
            )
            .unwrap();
        let current = store.action(action.draft).unwrap().unwrap();
        let Ok(Answer::Changed(fresh)) = store.issue_write(&rearm(&current), None) else {
            panic!("the last available value must commit");
        };
        assert_eq!(
            fresh,
            Action {
                version: StateVersion(expected_version),
                approval: Some(ApprovalRev(expected_approval)),
                ..action
            }
        );
        let before = (snapshot(&store.conn), events(&store));
        assert_eq!(before.0.1, sql(expected_approval));
        assert!(matches!(
            store.issue_write(&rearm(&fresh), None),
            Err(StoreError::Issue)
        ));
        assert_eq!((snapshot(&store.conn), events(&store)), before);
        assert_eq!(store.action(fresh.draft).unwrap(), Some(fresh));
        if expected_approval != max {
            assert_eq!(draft(&mut store, owner).approval, Some(ApprovalRev(3)));
        }
    }
}

#[test]
fn stages_and_marks_decode_only_within_their_bounds() {
    let numbers = |count: u64| {
        (1..=count)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let presend = |list: &str| {
        format!(r#"{{"kind":"preflight","unresolved":{{"at_s":1,"presend":{list}}}}}"#)
    };
    let pinned = [
        r#"{"kind":"draft"}"#.to_owned(),
        r#"{"kind":"preflight","unresolved":null}"#.to_owned(),
        format!(r#"{{"kind":"posting","mark":{B},"unresolved":{A}}}"#),
        r#"{"kind":"filed","number":7,"confirmed":true}"#.to_owned(),
        r#"{"kind":"failed","reason":"Interrupted"}"#.to_owned(),
        format!(
            r#"{{"kind":"unknown","mark":{C},"lookup":{{"Unplaced":{{"number":7,"url":"https://github.com/octo/repo/issues/7"}}}}}}"#
        ),
        r#"{"kind":"discarded"}"#.to_owned(),
        presend("[]"),
        presend(&format!("[{}]", numbers(200))),
        presend("[3,3]"),
    ];
    for json in &pinned {
        let stage: Stage = serde_json::from_str(json).expect(json);
        assert_eq!(&serde_json::to_string(&stage).expect("encodes"), json);
    }
    let refused = [
        presend(&format!("[{}]", numbers(201))),
        presend("[0]"),
        presend("[1.5]"),
        presend("[-3]"),
        presend(r#""3""#),
        presend("null"),
        r#"{"kind":"preflight","unresolved":{"at_s":0,"presend":[]}}"#.to_owned(),
        r#"{"kind":"preflight","unresolved":{"at_s":"5","presend":[]}}"#.to_owned(),
        r#"{"kind":"preflight","unresolved":{"presend":[]}}"#.to_owned(),
        r#"{"kind":"posting","mark":{"at_s":1,"presend":[1,2"#.to_owned(),
        r#"{"kind":"posting","unresolved":null}"#.to_owned(),
        r#"{"kind":"filed","number":0,"confirmed":false}"#.to_owned(),
        r#"{"kind":"sending"}"#.to_owned(),
        r#"{"kind":null}"#.to_owned(),
        "{}".to_owned(),
    ];
    for json in &refused {
        assert!(serde_json::from_str::<Stage>(json).is_err(), "{json}");
    }
}

#[test]
fn issue_actions_print_no_content() {
    let scratch = Scratch::new("actions-debug");
    let mut store = opened(&scratch);
    let thread = thread(&mut store);
    let account = GitHubUser {
        id: 7,
        login: GitHubLogin::new("sentinel").expect("login"),
    };
    let planted = RepoName::new("sentinel/sentinel").expect("repo");
    let created = store
        .create_draft(
            thread,
            &title("Title sentinel"),
            &body("Body sentinel"),
            Some(&planted),
            Some(&account),
        )
        .expect("draft");
    let id = created.draft;
    let repos = [planted.clone()];
    let live = Live {
        account: Some(&account),
        capacity: true,
        repos: &repos,
        repo_ids: &[],
    };
    let mut answers = Vec::new();
    let mut step = |answer: Result<Answer, StoreError>| {
        let answer = answer.expect("an answer");
        let (Answer::Changed(action) | Answer::Refused(_, action)) = &answer else {
            panic!("{answer:?}");
        };
        let action = action.clone();
        answers.push(answer);
        action
    };
    let file = Decision::File;
    let preflight = step(store.consent(id, &created.hash(), ApprovalRev(1), file, &live));
    let posting = step(store.preflighted(id, preflight.version, Ok(mark(A))));
    step(store.posted(id, posting.version, &uncertain()));
    let unplaced = step(store.looked_up(id, 1, Lookup::Unplaced(issue(7))));
    let offered = unplaced.approval.expect("offered");
    let confirm = Decision::ThisIsTheIssue;
    let refused = step(store.consent(id, &ContentHash([0; 32]), offered, confirm, &live));
    let offered = refused.approval.expect("offered");
    step(store.consent(id, &refused.hash(), offered, confirm, &live));
    let url = r#"{"Unplaced":{"number":7,"url":"https://github.com/sentinel/sentinel/issues/7"}}"#;
    seed(&store, id, &unknown(url), 1);
    let unplaced = store.action(id).expect("read").expect("live");
    seed(
        &store,
        id,
        r#"{"kind":"filed","number":7,"confirmed":true}"#,
        1,
    );
    let filed = store.action(id).expect("read").expect("live");
    let shown = format!("{:?}", unplaced.stage);
    let printed = [
        format!("{:?}", Answer::Changed(unplaced.clone())),
        format!("{:?}", Answer::Unchanged(unplaced)),
        format!("{:?}", Answer::Refused(IssueRefused::NotAllowedNow, filed)),
        format!("{:?} {}", StoreError::Issue, StoreError::Issue),
        format!("{live:?}"),
    ];
    let flow = answers.iter().map(|answer| format!("{answer:?}"));
    assert_eq!(flow.len(), 6);
    for text in printed.into_iter().chain(flow) {
        assert!(!text.contains("sentinel"), "{text}");
        assert!(
            !text.contains("115, 101, 110, 116, 105, 110, 101, 108"),
            "{text}"
        );
    }
    assert_eq!(shown, "Unknown(Unplaced(7))");
}

/// OSC-648: an attempt pending across V11 keeps its row with no repository id, and opening the
/// upgraded file again changes nothing.
#[test]
fn version_ten_attempts_gain_an_empty_repository_id_once() {
    let scratch = Scratch::new("actions-v10");
    std::fs::create_dir_all(scratch.dir()).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    schema::migrate_to(&mut conn, 10).expect("V10");
    conn.execute(
        "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1)",
        [],
    )
    .expect("an older thread");
    conn.execute(
        "INSERT INTO actions (thread, version, stage, repo, account_id, account_login, title,
         body, uncertain, created_ms) VALUES (5, 1, ?1, 'octo/repo', '7', 'octo', 'T', 'B', 1, 0)",
        [unknown(r#""Pending""#)],
    )
    .expect("an attempt pending across the upgrade");
    drop(conn);
    for open in ["upgrade", "reopen"] {
        let store = opened(&scratch);
        let (version, columns): (i64, i64) = store
            .conn
            .query_row(
                "SELECT (SELECT user_version FROM pragma_user_version),
                 (SELECT count(*) FROM pragma_table_info('actions') WHERE name = 'repo_id')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("schema");
        assert_eq!((version, columns), (11, 1), "{open}");
        let action = store.action(DraftId(1)).expect("reads").expect("kept");
        assert_eq!(
            (action.repo.as_ref().map(RepoName::as_str), action.repo_id),
            (Some("octo/repo"), None),
            "{open}"
        );
        assert!(
            constraint(
                &store
                    .conn
                    .execute("UPDATE actions SET repo_id = 0 WHERE id = 1", [])
            ),
            "{open}"
        );
    }
}
