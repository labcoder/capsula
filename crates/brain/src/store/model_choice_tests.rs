//! A thread's model choice by id: Set and Clear in one transaction with a revision, the by-id
//! read, Kumo's main chat like a task, and the V14 migration.

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ConnectionKind, Effort, ModelChoice, ModelSlug, RunId, SavedChoice,
    ThreadId,
};
use rusqlite::Connection;

use super::context::Snapshot;
use super::{RunEnd, Store, schema};
use crate::test_support::Scratch;

fn opened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
}

fn task(store: &mut Store) -> ThreadId {
    let (thread, run) = store.high_water().expect("ids");
    let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(thread, Some("task"), run, "words")
        .expect("turn");
    store.end_run(thread, run, &[], RunEnd::Done).expect("end");
    thread
}

fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(model).expect("valid"),
        effort,
        connection: ConnectionKind::ChatGpt,
    }
}

fn read(store: &Store, thread: ThreadId) -> ChoiceRead {
    let snapshot = Snapshot::begin(&store.conn).expect("snapshot");
    let read = snapshot.model_choice(thread).expect("read");
    snapshot.end().expect("end");
    read
}

fn columns(
    store: &Store,
    thread: ThreadId,
) -> (Option<String>, Option<String>, Option<String>, i64) {
    store
        .conn
        .query_row(
            "SELECT model, effort, connection, model_choice_rev FROM threads WHERE id = ?1",
            [i64::try_from(thread.0).expect("small")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("row")
}

fn found(choice: SavedChoice, revision: u64) -> ChoiceRead {
    ChoiceRead::Found {
        choice,
        revision: ChoiceRev(revision),
    }
}

/// Clear writes three NULLs, never a copy of a default, and each write advances the revision.
/// Kumo's main chat takes the same writes as a task, and neither touches the other.
#[test]
fn set_and_clear_commit_with_a_revision_on_main_and_a_task_alike() {
    let scratch = Scratch::new("model-choice-set-clear");
    let mut store = opened(&scratch);
    let main = store.main_thread().expect("main");
    let task = task(&mut store);
    let picked = choice("chosen-model", Effort::High);
    for (thread, other) in [(main, task), (task, main)] {
        let before = columns(&store, other);
        assert_eq!(read(&store, thread), found(SavedChoice::Inherit, 0));
        let set = store.set_model(thread, Some(&picked)).expect("set");
        assert_eq!(set, found(SavedChoice::Chosen(picked.clone()), 1));
        assert_eq!(read(&store, thread), set);
        assert_eq!(
            columns(&store, thread),
            (
                Some("chosen-model".to_owned()),
                Some("high".to_owned()),
                Some("chatgpt".to_owned()),
                1
            )
        );
        let cleared = store.set_model(thread, None).expect("clear");
        assert_eq!(cleared, found(SavedChoice::Inherit, 2));
        assert_eq!(read(&store, thread), cleared);
        assert_eq!(columns(&store, thread), (None, None, None, 2));
        assert_eq!(
            columns(&store, other),
            before,
            "the other chat is untouched"
        );
    }
}

/// After a Clear a run uses whichever global default is current when it starts, not the one
/// current at the Clear.
#[test]
fn a_cleared_chat_follows_a_later_global_default() {
    let scratch = Scratch::new("model-choice-follow");
    let mut store = opened(&scratch);
    let task = task(&mut store);
    store
        .set_model(task, Some(&choice("pinned", Effort::Low)))
        .expect("set");
    store.set_model(task, None).expect("clear");
    let snapshot = Snapshot::begin(&store.conn).expect("snapshot");
    for global in [
        choice("global-a", Effort::Medium),
        choice("global-b", Effort::XHigh),
    ] {
        assert_eq!(
            snapshot
                .stored_choice(task)
                .expect("stored")
                .execution(Some(global.clone()))
                .expect("runs"),
            Some(global)
        );
    }
}

#[test]
fn a_missing_or_deleting_thread_reads_and_writes_as_missing() {
    let scratch = Scratch::new("model-choice-missing");
    let mut store = opened(&scratch);
    let deleting = task(&mut store);
    store.begin_delete(deleting).expect("deleting");
    for thread in [deleting, ThreadId(999), ThreadId(u64::MAX)] {
        assert_eq!(read(&store, thread), ChoiceRead::Missing);
        assert_eq!(
            store.set_model(thread, None).expect("clear"),
            ChoiceRead::Missing
        );
    }
    assert_eq!(columns(&store, deleting).3, 0, "nothing was written");
}

#[test]
fn a_revision_that_cant_advance_refuses_and_writes_nothing() {
    let scratch = Scratch::new("model-choice-overflow");
    let mut store = opened(&scratch);
    let task = task(&mut store);
    store
        .conn
        .execute(
            "UPDATE threads SET model_choice_rev = ?2 WHERE id = ?1",
            [i64::try_from(task.0).expect("small"), i64::MAX - 1],
        )
        .expect("near the end");
    let picked = choice("chosen-model", Effort::High);
    assert_eq!(
        store.set_model(task, Some(&picked)).expect("last"),
        found(
            SavedChoice::Chosen(picked),
            u64::try_from(i64::MAX).expect("fits")
        )
    );
    let full = columns(&store, task);
    assert_eq!(
        store.set_model(task, None).expect("refused"),
        ChoiceRead::Failed
    );
    assert_eq!(columns(&store, task), full);
}

/// An unknown effort or connection reads as unavailable, never as Inherit or a guess.
#[test]
fn an_unreadable_saved_choice_reads_as_unavailable_with_its_revision() {
    let scratch = Scratch::new("model-choice-unavailable");
    let mut store = opened(&scratch);
    for (model, effort, connection) in [
        (Some("same-slug"), Some("ultra"), Some("chatgpt")),
        (Some("same-slug"), Some("high"), Some("unknown")),
    ] {
        let task = task(&mut store);
        store
            .conn
            .execute(
                "UPDATE threads SET model = ?2, effort = ?3, connection = ?4, model_choice_rev = 5
                 WHERE id = ?1",
                rusqlite::params![
                    i64::try_from(task.0).expect("small"),
                    model,
                    effort,
                    connection
                ],
            )
            .expect("planted");
        assert_eq!(read(&store, task), found(SavedChoice::Unavailable, 5));
    }
}

/// A V13 file keeps its saved and inheriting rows, both at revision 0.
#[test]
fn the_v14_migration_keeps_saved_choices_at_revision_zero() {
    let scratch = Scratch::new("model-choice-v14");
    std::fs::create_dir_all(scratch.db().parent().expect("folder")).expect("folder");
    let mut conn = Connection::open(scratch.db()).expect("file");
    schema::migrate_to(&mut conn, 13).expect("V13");
    conn.execute(
        "INSERT INTO threads (id, title, created_ms, updated_ms, model, effort, connection)
        VALUES (2, 'saved', 1, 1, 'same-slug', 'xhigh', 'chatgpt'), (3, 'unset', 1, 1, NULL, NULL, NULL)",
        [],
    )
    .expect("old rows");
    drop(conn);
    let store = opened(&scratch);
    assert_eq!(
        read(&store, ThreadId(2)),
        found(SavedChoice::Chosen(choice("same-slug", Effort::XHigh)), 0)
    );
    assert_eq!(read(&store, ThreadId(3)), found(SavedChoice::Inherit, 0));
}
