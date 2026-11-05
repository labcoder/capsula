//! The main thread, kinds, titles and model choices in the store: the migration, the plain
//! list's pinned first row, and the title and deletion rules.

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    DataToShell, Effort, MAX_PAGE_BYTES, MAX_PAGE_ROWS, ModelChoice, ModelSlug, PageOf, RunId,
    ThreadCursor, ThreadId, ThreadKind, ThreadRow, ThreadTitle,
};
use rusqlite::{Connection, OptionalExtension as _};

use super::context::{FirstAsk, Snapshot};
use super::tests::{purge, raw_bytes};
use super::{Generation, RunEnd, Store, StoreError, Swap, TitleSource, schema};
use crate::test_support::Scratch;

/// A store as the brain opens it: recovered, so the main thread exists.
fn opened(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store
}

fn sql(thread: ThreadId) -> i64 {
    i64::try_from(thread.0).expect("small")
}

/// A task with the next ids whose `updated_ms` is its id, so the list's order is fixed.
fn task(store: &mut Store, title: &str) -> ThreadId {
    let (thread, run) = store.high_water().expect("ids");
    let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(thread, Some(title), run, "needle")
        .expect("turn");
    store.end_run(thread, run, &[], RunEnd::Done).expect("end");
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = id WHERE id = ?1",
            [sql(thread)],
        )
        .expect("stable time");
    thread
}

/// A thread's stored title and where it came from; `None` once its row is gone.
fn title(store: &Store, thread: ThreadId) -> Option<(String, String)> {
    store
        .conn
        .query_row(
            "SELECT title, title_source FROM threads WHERE id = ?1",
            [sql(thread)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("row")
}

fn titled(text: &str, source: &str) -> (String, String) {
    (text.to_owned(), source.to_owned())
}

fn rename(store: &mut Store, thread: ThreadId, text: &str, source: TitleSource) {
    let text = ThreadTitle::new(text).expect("valid");
    store.set_title(thread, &text, source).expect("set");
}

fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(model).expect("valid"),
        effort,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

/// The plain list's page after `after`, as the shell reads it.
fn list(store: &Store, after: Option<ThreadCursor>) -> (Vec<ThreadRow>, Option<ThreadCursor>) {
    let page = PageOf::Threads {
        search: None,
        after,
    };
    let reply = store.pages().read_page(1, page).expect("page");
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
    let DataToShell::ThreadList { threads, next, .. } = reply else {
        panic!("a list")
    };
    (threads, next)
}

fn ids_and_kinds(rows: &[ThreadRow]) -> Vec<(u64, ThreadKind)> {
    rows.iter().map(|row| (row.id.0, row.kind)).collect()
}

fn holds(scratch: &Scratch, needle: &str) -> bool {
    raw_bytes(&scratch.db())
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A `threads` row: id, kind, title, title source, model and effort.
type StoredThread = (i64, String, String, String, Option<String>, Option<String>);

/// Every earlier version, so the current merged schema and any combined prefix of later
/// migrations are covered as they're appended.
#[test]
fn every_older_schema_upgrades_to_one_main_thread_and_keeps_its_rows() {
    for version in 0..schema::MIGRATIONS.len() {
        let scratch = Scratch::new(&format!("upgrade-{version}"));
        std::fs::create_dir_all(scratch.db().parent().expect("a folder")).expect("folder");
        let mut conn = Connection::open(scratch.db()).expect("file");
        schema::migrate_to(&mut conn, version).expect("older schema");
        let kept = version >= 1;
        if kept {
            conn.execute(
                "INSERT INTO threads (id, title, created_ms, updated_ms) VALUES (5, 'kept', 1, 1)",
                [],
            )
            .expect("an older row");
        }
        drop(conn);
        // A second start finds the main thread and makes no other.
        for _ in 0..2 {
            let store = opened(&scratch);
            let rows: Vec<StoredThread> = store
                .conn
                .prepare(
                    "SELECT id, kind, title, title_source, model, effort FROM threads ORDER BY id",
                )
                .expect("query")
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .expect("rows")
                .collect::<Result<_, _>>()
                .expect("rows");
            let main = |id| {
                (
                    id,
                    "main".to_owned(),
                    String::new(),
                    "words".to_owned(),
                    None,
                    None,
                )
            };
            let (expected, main_id) = if kept {
                let older = (
                    5,
                    "thread".to_owned(),
                    "kept".to_owned(),
                    "words".to_owned(),
                    None,
                    None,
                );
                (vec![older, main(6)], 6)
            } else {
                (vec![main(1)], 1)
            };
            assert_eq!(rows, expected, "from version {version}");
            assert_eq!(store.main_thread().expect("main"), ThreadId(main_id));
            let current: i64 = store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("version");
            assert_eq!(
                current,
                i64::try_from(schema::MIGRATIONS.len()).expect("small")
            );
        }
    }
}

#[test]
fn a_main_thread_insert_that_fails_leaves_none_and_the_next_start_creates_one() {
    let scratch = Scratch::new("main-crash");
    let mut store = Store::open(&scratch.db()).expect("store");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER no_main BEFORE INSERT ON threads WHEN new.kind = 'main'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .expect("injected failure");
    assert!(store.recover().is_err());
    assert!(matches!(store.main_thread(), Err(StoreError::NoThread)));
    store
        .conn
        .execute_batch("DROP TRIGGER no_main")
        .expect("failure lifted");
    drop(store);
    let store = opened(&scratch);
    let mains: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM threads WHERE kind = 'main'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(mains, 1);
    assert_eq!(store.main_thread().expect("main"), ThreadId(1));
}

#[test]
fn the_schema_allows_one_main_thread_and_a_model_only_with_its_effort() {
    let scratch = Scratch::new("main-constraints");
    let mut store = opened(&scratch);
    let thread = task(&mut store, "task");
    let second_main = store.conn.execute(
        "INSERT INTO threads (title, kind, created_ms, updated_ms) VALUES ('', 'main', 1, 1)",
        [],
    );
    assert!(second_main.is_err());
    for half in ["model = 'gpt-6'", "effort = 'high'"] {
        let set = store.conn.execute(
            &format!("UPDATE threads SET {half} WHERE id = ?1"),
            [sql(thread)],
        );
        assert!(set.is_err(), "{half} alone was stored");
    }
    let both = store.conn.execute(
        "UPDATE threads SET model = 'gpt-6', effort = 'high' WHERE id = ?1",
        [sql(thread)],
    );
    assert_eq!(both.expect("both set"), 1);
}

#[test]
fn page_one_starts_with_kumo_and_no_later_page_repeats_it() {
    let scratch = Scratch::new("main-pages");
    let mut store = opened(&scratch);
    for _ in 0..7 {
        task(&mut store, "task");
    }
    // Kumo is the oldest thread, yet first; then the newest, so its cursor would skip tasks.
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 0 WHERE kind = 'main'", [])
        .expect("oldest");
    let (first, next) = store.pages().thread_page(None, 3).expect("first");
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = ?1 WHERE kind = 'main'",
            [i64::MAX],
        )
        .expect("newest");
    let (second, next) = store.pages().thread_page(next, 3).expect("second");
    let (third, next) = store.pages().thread_page(next, 3).expect("third");
    let thread = ThreadKind::Thread;
    assert_eq!(
        [
            ids_and_kinds(&first),
            ids_and_kinds(&second),
            ids_and_kinds(&third)
        ],
        [
            vec![(1, ThreadKind::Main), (8, thread), (7, thread)],
            vec![(6, thread), (5, thread), (4, thread)],
            vec![(3, thread), (2, thread)],
        ]
    );
    assert_eq!(next, None);
}

#[test]
fn a_list_of_only_kumo_ends_there_and_a_full_first_page_counts_it() {
    let scratch = Scratch::new("main-alone");
    let mut store = opened(&scratch);
    let (rows, next) = list(&store, None);
    assert_eq!(ids_and_kinds(&rows), [(1, ThreadKind::Main)]);
    assert_eq!(
        (rows[0].title.as_str(), &rows[0].choice, next),
        ("", &None, None)
    );
    for _ in 0..MAX_PAGE_ROWS + 10 {
        task(&mut store, "task");
    }
    let (rows, next) = list(&store, None);
    assert_eq!(rows.len(), MAX_PAGE_ROWS);
    assert_eq!(rows[0].kind, ThreadKind::Main);
    assert_eq!(next.map(|key| key.id), Some(rows[MAX_PAGE_ROWS - 1].id));
    let (rows, next) = list(&store, next);
    let rest: Vec<u64> = (2..=12).rev().collect();
    assert_eq!(rows.iter().map(|row| row.id.0).collect::<Vec<_>>(), rest);
    assert_eq!(next, None);
}

/// CAPS-65 H1: pinning Kumo must not cost the first page its task.
#[test]
fn an_impossible_title_after_kumo_is_shortened_and_the_list_goes_on() {
    let scratch = Scratch::new("main-title");
    let mut store = opened(&scratch);
    let older = task(&mut store, "older");
    let long = "界".repeat(100_000);
    let newest = task(&mut store, &long);
    let (rows, next) = list(&store, None);
    assert_eq!(
        ids_and_kinds(&rows),
        [(1, ThreadKind::Main), (newest.0, ThreadKind::Thread)]
    );
    assert_eq!(rows[0].title, "");
    assert!(!rows[1].title.is_empty() && rows[1].title.len() < long.len());
    assert!(long.starts_with(&rows[1].title));
    assert_eq!(next.map(|key| key.id), Some(newest));
    let (rows, next) = list(&store, next);
    assert_eq!(ids_and_kinds(&rows), [(older.0, ThreadKind::Thread)]);
    assert_eq!(next, None);
}

#[test]
fn search_rows_carry_kind_and_choice_and_rank_kumo_like_any_thread() {
    let scratch = Scratch::new("main-search");
    let mut store = opened(&scratch);
    let main = store.main_thread().expect("main");
    let thread = task(&mut store, "task");
    let picked = choice("gpt-6.1-sol", Effort::Low);
    store.set_model(thread, &picked).expect("model");
    let run = RunId(store.high_water().expect("ids").1.0 + 1);
    store
        .begin_turn(main, None, run, "needle needle needle")
        .expect("a turn with Kumo");
    let (rows, _) = store.pages().search_page("needle", 50).expect("search");
    let found: Vec<_> = rows
        .iter()
        .map(|row| (row.id, row.kind, row.choice.clone()))
        .collect();
    assert_eq!(
        found,
        [
            (main, ThreadKind::Main, None),
            (thread, ThreadKind::Thread, Some(picked)),
        ]
    );
}

#[test]
fn a_user_title_always_wins_and_a_model_title_only_replaces_first_words() {
    let scratch = Scratch::new("titles");
    let mut store = opened(&scratch);
    let (a, b) = (task(&mut store, "words a"), task(&mut store, "words b"));
    let current = TitleSource::Model(Generation(0));
    rename(&mut store, a, "Model a", current);
    assert_eq!(title(&store, a), Some(titled("Model a", "model")));
    rename(&mut store, a, "Model again", current);
    assert_eq!(title(&store, a), Some(titled("Model a", "model")));
    rename(&mut store, a, "User a", TitleSource::User);
    assert_eq!(title(&store, a), Some(titled("User a", "user")));
    rename(&mut store, b, "User b", TitleSource::User);
    rename(&mut store, b, "Late model", current);
    assert_eq!(title(&store, b), Some(titled("User b", "user")));
    rename(&mut store, b, "User again", TitleSource::User);
    assert_eq!(title(&store, b), Some(titled("User again", "user")));
}

/// CAPS-65 M4: a model title from before a deletion never lands.
#[test]
fn a_model_title_from_another_generation_or_after_a_delete_changes_nothing() {
    let scratch = Scratch::new("titles-deleted");
    let mut store = opened(&scratch);
    let old = task(&mut store, "words");
    store
        .conn
        .execute(
            "UPDATE threads SET deletion_generation = 2 WHERE id = ?1",
            [sql(old)],
        )
        .expect("a later generation");
    rename(&mut store, old, "Old", TitleSource::Model(Generation(1)));
    assert_eq!(title(&store, old), Some(titled("words", "words")));
    rename(
        &mut store,
        old,
        "Current",
        TitleSource::Model(Generation(2)),
    );
    assert_eq!(title(&store, old), Some(titled("Current", "model")));

    let hidden = task(&mut store, "words");
    let generation = store.begin_delete(hidden).expect("hidden");
    rename(&mut store, hidden, "Late", TitleSource::Model(generation));
    rename(&mut store, hidden, "Late", TitleSource::User);
    assert_eq!(title(&store, hidden), Some(titled("words", "words")));

    let removed = task(&mut store, "words");
    let generation = store.begin_delete(removed).expect("hidden");
    store.finish_delete(removed).expect("removed");
    rename(&mut store, removed, "Late", TitleSource::Model(generation));
    rename(&mut store, removed, "Late", TitleSource::User);
    assert_eq!(title(&store, removed), None);
}

#[test]
fn a_later_turn_keeps_a_user_or_model_title_and_kumos_title_stays_empty() {
    let scratch = Scratch::new("titles-turn");
    let mut store = opened(&scratch);
    let (user, model) = (task(&mut store, "words u"), task(&mut store, "words m"));
    rename(&mut store, user, "Mine", TitleSource::User);
    rename(&mut store, model, "Its", TitleSource::Model(Generation(0)));
    for thread in [user, model] {
        let run = RunId(store.high_water().expect("ids").1.0 + 1);
        store.begin_turn(thread, None, run, "more").expect("turn");
    }
    assert_eq!(title(&store, user), Some(titled("Mine", "user")));
    assert_eq!(title(&store, model), Some(titled("Its", "model")));
    let main = store.main_thread().expect("main");
    rename(&mut store, main, "Not Kumo", TitleSource::User);
    assert_eq!(title(&store, main), Some(titled("", "words")));
}

#[test]
fn a_model_choice_round_trips_through_a_page_and_a_restart() {
    let scratch = Scratch::new("choice");
    let mut store = opened(&scratch);
    let thread = task(&mut store, "task");
    let hidden = task(&mut store, "hidden");
    let picked = choice("gpt-6.1-sol", Effort::XHigh);
    store.set_model(thread, &picked).expect("model");
    store.begin_delete(hidden).expect("hidden");
    store.set_model(hidden, &picked).expect("nothing to change");
    let hidden_model: Option<String> = store
        .conn
        .query_row(
            "SELECT model FROM threads WHERE id = ?1",
            [sql(hidden)],
            |row| row.get(0),
        )
        .expect("row");
    assert_eq!(hidden_model, None);
    drop(store);
    let store = opened(&scratch);
    let (rows, _) = list(&store, None);
    let choices: Vec<_> = rows
        .iter()
        .map(|row| (row.id, row.choice.clone()))
        .collect();
    assert_eq!(choices, [(ThreadId(1), None), (thread, Some(picked))]);
}

/// CAPS-90 M1: an effort this build doesn't know, an older build's `minimal` or a newer build's,
/// reads as no choice instead of failing the page, and the other rows are unchanged.
#[test]
fn an_unknown_stored_effort_reads_as_no_choice() {
    let scratch = Scratch::new("choice-unknown-effort");
    let mut store = opened(&scratch);
    let newer = task(&mut store, "newer");
    let older = task(&mut store, "older");
    let kept = task(&mut store, "kept");
    let picked = choice("gpt-6.1-sol", Effort::Max);
    store.set_model(kept, &picked).expect("model");
    for (thread, effort) in [(newer, "ultra"), (older, "minimal")] {
        store
            .conn
            .execute(
                "UPDATE threads SET model = 'gpt-6.1-sol', effort = ?2 WHERE id = ?1",
                rusqlite::params![sql(thread), effort],
            )
            .expect("planted");
    }
    let (rows, _) = list(&store, None);
    let choices: Vec<_> = rows
        .iter()
        .map(|row| (row.id, row.choice.clone()))
        .collect();
    assert_eq!(
        choices,
        [
            (ThreadId(1), None),
            (kept, Some(picked)),
            (older, None),
            (newer, None)
        ]
    );
}

/// CAPS-152: a row whose slug no build writes still fails the page, even when its effort is one
/// this build doesn't know, since the slug is checked first.
#[test]
fn a_bad_slug_fails_the_page_even_with_an_unknown_effort() {
    let scratch = Scratch::new("choice-bad-slug-unknown-effort");
    let mut store = opened(&scratch);
    let planted = task(&mut store, "planted");
    store
        .conn
        .execute(
            "UPDATE threads SET model = 'Not A Slug', effort = 'ultra' WHERE id = ?1",
            rusqlite::params![sql(planted)],
        )
        .expect("planted");
    let page = PageOf::Threads {
        search: None,
        after: None,
    };
    assert!(matches!(
        store.pages().read_page(1, page),
        Err(StoreError::Read)
    ));
}

/// CAPS-65 H2: deleting a task's row takes its title and model with it, and the purge's copy
/// carries the kept rows' columns over.
#[test]
fn a_deleted_tasks_title_and_model_leave_the_files_and_a_kept_choice_survives() {
    const TITLE: &str = "zqxrenamedmarker";
    const SLUG: &str = "zqxslugmarker";
    let scratch = Scratch::new("choice-purge");
    let mut store = opened(&scratch);
    let kept = task(&mut store, "kept");
    let kept_choice = choice("kept-model", Effort::Low);
    store.set_model(kept, &kept_choice).expect("model");
    let doomed = task(&mut store, "doomed");
    rename(&mut store, doomed, &TITLE.to_uppercase(), TitleSource::User);
    store
        .set_model(doomed, &choice(SLUG, Effort::High))
        .expect("model");
    assert!(holds(&scratch, TITLE) && holds(&scratch, SLUG), "planted");
    assert_eq!(
        store.delete(doomed).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    assert_eq!(purge(&mut store), Swap::Done);
    assert!(
        !holds(&scratch, TITLE) && !holds(&scratch, SLUG),
        "found while open"
    );
    drop(store);
    assert!(
        !holds(&scratch, TITLE) && !holds(&scratch, SLUG),
        "found after close"
    );
    let store = opened(&scratch);
    let (rows, _) = list(&store, None);
    let choices: Vec<_> = rows
        .iter()
        .map(|row| (row.id, row.choice.clone()))
        .collect();
    assert_eq!(choices, [(ThreadId(1), None), (kept, Some(kept_choice))]);
}

#[test]
fn the_first_ask_reads_only_while_the_title_is_first_words_and_a_title_says_if_it_applied() {
    let scratch = Scratch::new("first-ask");
    let mut store = opened(&scratch);
    let task = task(&mut store, "words");
    let (_, run) = store.high_water().expect("ids");
    store
        .begin_turn(task, None, RunId(run.0 + 1), "later")
        .expect("turn");
    let read = |store: &Store, thread: ThreadId| {
        let snapshot = Snapshot::begin(&store.conn).expect("snapshot");
        let ask = snapshot.first_ask(thread).expect("read");
        snapshot.end().expect("end");
        ask
    };
    assert_eq!(
        read(&store, task),
        Some(FirstAsk {
            generation: Generation(0),
            text: "needle".to_owned(),
        }),
        "the first ask, not the latest"
    );
    let main = store.main_thread().expect("main");
    assert_eq!(read(&store, main), None, "Kumo has no title job");
    let model = ThreadTitle::new("Model").expect("valid");
    assert!(
        !store
            .set_title(task, &model, TitleSource::Model(Generation(1)))
            .expect("set"),
        "another generation's title doesn't apply"
    );
    assert!(
        store
            .set_title(task, &model, TitleSource::Model(Generation(0)))
            .expect("set"),
        "the read's generation applies over first words"
    );
    assert_eq!(read(&store, task), None, "the title isn't first words now");
    assert!(
        !store
            .set_title(task, &model, TitleSource::Model(Generation(0)))
            .expect("set"),
        "a model title replaces only first words"
    );
    assert!(
        store
            .set_title(task, &model, TitleSource::User)
            .expect("set"),
        "a user's title always applies"
    );
    assert!(
        !store
            .set_title(main, &model, TitleSource::User)
            .expect("set"),
        "Kumo's title stays empty"
    );
}

#[test]
fn titles_and_their_sources_survive_a_restart() {
    let scratch = Scratch::new("title-restart");
    let mut store = opened(&scratch);
    let task = task(&mut store, "words");
    drop(store);
    let mut store = opened(&scratch);
    assert_eq!(
        title(&store, task),
        Some(titled("words", "words")),
        "a job that failed or was cancelled leaves the first words"
    );
    rename(
        &mut store,
        task,
        "Model title",
        TitleSource::Model(Generation(0)),
    );
    drop(store);
    let mut store = opened(&scratch);
    assert_eq!(title(&store, task), Some(titled("Model title", "model")));
    rename(&mut store, task, "Mine", TitleSource::User);
    let late = ThreadTitle::new("Late model").expect("valid");
    assert!(
        !store
            .set_title(task, &late, TitleSource::Model(Generation(0)))
            .expect("set")
    );
    drop(store);
    let store = opened(&scratch);
    assert_eq!(title(&store, task), Some(titled("Mine", "user")));
}
