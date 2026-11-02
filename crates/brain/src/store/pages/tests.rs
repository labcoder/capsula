use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::*;
use crate::store::{EventKind, NewEvent, RunEnd, Swap, purge};
use crate::test_support::Scratch;

fn add(store: &mut Store, title: &str, text: &str) -> ThreadId {
    let (thread, run) = store.high_water().expect("ids");
    let thread = ThreadId(thread.0 + 1);
    let run = RunId(run.0 + 1);
    store
        .begin_turn(thread, Some(title), run, text)
        .expect("turn");
    store.end_run(thread, run, &[], RunEnd::Done).expect("end");
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = ?1 WHERE id = ?1",
            [bounded_id(thread.0)],
        )
        .expect("stable time");
    thread
}

fn cursor(row: &ThreadRow) -> ThreadCursor {
    ThreadCursor {
        updated_ms: row.updated_ms,
        id: row.id,
        rank_bits: 0,
    }
}

fn ids(rows: &[ThreadRow]) -> Vec<u64> {
    rows.iter().map(|row| row.id.0).collect()
}

fn list(store: &Store, search: Option<&str>, after: Option<ThreadCursor>) -> DataToShell {
    store
        .read_page(
            u32::MAX,
            PageOf::Threads {
                search: search.map(str::to_owned),
                after,
            },
        )
        .expect("page")
}

fn events(store: &Store, thread: ThreadId, before: Option<EventId>) -> DataToShell {
    store
        .read_page(u32::MAX, PageOf::Events { thread, before })
        .expect("page")
}

#[test]
fn keyset_pages_keep_original_threads_once_while_new_threads_arrive() {
    let scratch = Scratch::new("pages-keyset");
    let mut store = Store::open(&scratch.db()).expect("store");
    for _ in 0..7 {
        add(&mut store, "title", "text");
    }
    let (first, mut next) = store.thread_page(None, 2).expect("first");
    let mut seen = ids(&first);
    while next.is_some() {
        add(&mut store, "newer", "newer");
        let (rows, cursor) = store.thread_page(next, 2).expect("next");
        seen.extend(ids(&rows));
        next = cursor;
    }
    assert_eq!(seen, vec![7, 6, 5, 4, 3, 2, 1]);
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 99", [])
        .expect("ties");
    let (first, next) = store.thread_page(None, 2).expect("tied first");
    let (second, _) = store.thread_page(next, 2).expect("tied next");
    assert_eq!(ids(&first), vec![10, 9]);
    assert_eq!(ids(&second), vec![8, 7]);
}

#[test]
fn updated_threads_move_above_the_cursor_without_duplicates() {
    let scratch = Scratch::new("pages-moved");
    let mut store = Store::open(&scratch.db()).expect("store");
    for _ in 0..6 {
        add(&mut store, "title", "text");
    }
    let (rows, next) = store.thread_page(None, 2).expect("first");
    assert_eq!(ids(&rows), vec![6, 5]);
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 100 WHERE id = 5", [])
        .expect("seen moves");
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 101 WHERE id = 1", [])
        .expect("unseen moves");
    let (later, next) = store.thread_page(next, 50).expect("later");
    assert_eq!(ids(&later), vec![4, 3, 2]);
    assert_eq!(next, None);
    assert_eq!(
        ids(&store.thread_page(None, 2).expect("fresh").0),
        vec![1, 5]
    );
}

#[test]
fn deleting_threads_are_hidden_from_list_search_and_events() {
    let scratch = Scratch::new("pages-deleting");
    let mut store = Store::open(&scratch.db()).expect("store");
    let hidden = add(&mut store, "hidden", "the red fox");
    let live = add(&mut store, "live", "the red fox");
    store.begin_delete(hidden).expect("hide");
    assert_eq!(
        ids(&store.thread_page(None, 50).expect("list").0),
        vec![live.0]
    );
    assert_eq!(
        ids(&store.search_page("red fox", 50).expect("search").0),
        vec![live.0]
    );
    assert_eq!(
        store.events_page(hidden, None, 50).expect("hidden"),
        (vec![], None)
    );
    for thread in [ThreadId(999), ThreadId(u64::MAX)] {
        assert_eq!(
            store.events_page(thread, None, 50).expect("missing"),
            (vec![], None)
        );
    }
}

#[test]
fn phrase_search_pages_follow_rank_and_deduplicate_threads() {
    let scratch = Scratch::new("pages-search");
    let mut store = Store::open(&scratch.db()).expect("store");
    add(&mut store, "best", &"red fox fence ".repeat(15));
    let weak = format!("red fox {}", "filler ".repeat(100));
    add(&mut store, "weak", &weak);
    add(&mut store, "weak tie", &weak);
    store
        .begin_turn(ThreadId(1), None, RunId(4), "red fox")
        .expect("second match");
    store
        .end_run(ThreadId(1), RunId(4), &[], RunEnd::Done)
        .expect("end");
    let (rows, next) = store.search_page("RED fox", 1).expect("best");
    assert_eq!(ids(&rows), vec![1]);
    let (rows, next) = store
        .search_page_after("RED fox", next, 1)
        .expect("tie first");
    assert_eq!(ids(&rows), vec![3]);
    let (rows, next) = store
        .search_page_after("RED fox", next, 1)
        .expect("tie second");
    assert_eq!(ids(&rows), vec![2]);
    assert_eq!(next, None);
    for words in ["fox red", "", "\0", "red\0fox", r#"" OR * NEAR("#] {
        assert_eq!(
            store.search_page(words, 50).expect("literal phrase"),
            (vec![], None)
        );
    }
}

#[test]
fn a_search_continues_after_its_cursor_thread_is_deleted_and_refuses_a_non_finite_rank() {
    let scratch = Scratch::new("pages-search-deleted");
    let mut store = Store::open(&scratch.db()).expect("store");
    add(&mut store, "best", &"red fox fence ".repeat(15));
    let weak = format!("red fox {}", "filler ".repeat(100));
    add(&mut store, "weak", &weak);
    add(&mut store, "weak tie", &weak);
    let (rows, next) = store.search_page("red fox", 2).expect("first");
    assert_eq!(ids(&rows), vec![1, 3]);
    store
        .begin_delete(ThreadId(3))
        .expect("hide the cursor's thread");
    let (rows, next) = store
        .search_page_after("red fox", next, 2)
        .expect("continued");
    assert_eq!(ids(&rows), vec![2]);
    assert_eq!(next, None);
    for (search, rank) in [
        ("red fox", f64::NAN),
        ("red fox", f64::INFINITY),
        ("red fox", f64::NEG_INFINITY),
        ("\0", f64::NAN),
    ] {
        let page = PageOf::Threads {
            search: Some(search.to_owned()),
            after: Some(ThreadCursor {
                updated_ms: 0,
                id: ThreadId(1),
                rank_bits: rank.to_bits(),
            }),
        };
        assert!(matches!(store.read_page(1, page), Err(StoreError::Read)));
    }
}

#[test]
fn a_search_continues_after_an_unrelated_write_moves_every_rank() {
    let scratch = Scratch::new("pages-search-rescored");
    let mut store = Store::open(&scratch.db()).expect("store");
    for _ in 0..54 {
        add(&mut store, "needle", "needle");
    }
    let (thread, run) = (ThreadId(55), RunId(55));
    store
        .begin_turn(thread, Some("active"), run, "needle")
        .expect("an active run");
    let (rows, next) = store.search_page("needle", 50).expect("first");
    assert_eq!(ids(&rows), (6..=55).rev().collect::<Vec<_>>());
    let other = "other ".repeat(57);
    let answer = NewEvent {
        kind: EventKind::AssistantText,
        text: Some(&other),
        body: None,
    };
    store.append(thread, run, &[answer]).expect("an answer");
    let (rows, next) = store
        .search_page_after("needle", next, 50)
        .expect("continued");
    assert_eq!(ids(&rows), vec![5, 4, 3, 2, 1]);
    assert_eq!(next, None);
}

#[test]
fn newest_events_continue_exactly_before_the_last_returned_id_and_skip_revealed_sets() {
    let scratch = Scratch::new("pages-events");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = add(&mut store, "title", "oldest");
    store
        .begin_turn(thread, None, RunId(2), "second")
        .expect("turn");
    let kinds = [
        EventKind::AssistantText,
        EventKind::SceneStep,
        EventKind::Revealed,
    ];
    for kind in kinds {
        store
            .append(
                thread,
                RunId(2),
                &[NewEvent {
                    kind,
                    text: None,
                    body: Some("{}"),
                }],
            )
            .expect("event");
    }
    let (rows, before) = store.events_page(thread, None, 2).expect("newest");
    assert_eq!(
        rows.iter().map(|event| event.id.0).collect::<Vec<_>>(),
        vec![4, 3]
    );
    assert_eq!(rows[0].kind, PageEventKind::SceneStep);
    assert_eq!(rows[1].kind, PageEventKind::AssistantText);
    assert_eq!(rows[0].text, "");
    assert_eq!(rows[0].run, Some(RunId(2)));
    assert_eq!(before, Some(EventId(3)));
    let (rows, before) = store.events_page(thread, before, 2).expect("older");
    assert_eq!(
        rows.iter().map(|event| event.id.0).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(rows[0].kind, PageEventKind::User);
    assert_eq!(rows[0].text, "second");
    assert_eq!(rows[1].text, "oldest");
    assert_eq!(before, None);
}

#[test]
fn oversized_unicode_event_is_cut_to_the_full_frame_budget_and_can_continue() {
    let scratch = Scratch::new("pages-unicode");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = add(&mut store, "title", "older");
    let text = "🙂漢é".repeat(40_000);
    store
        .begin_turn(thread, None, RunId(2), &text)
        .expect("huge event");
    let mut reply = events(&store, thread, None);
    let DataToShell::Page {
        request,
        events: rows,
        before,
        ..
    } = &reply
    else {
        panic!("events")
    };
    assert_eq!(*request, u32::MAX);
    assert_eq!(rows.len(), 1);
    assert_eq!(*before, Some(EventId(2)));
    assert!(rows[0].cut);
    assert!(text.starts_with(&rows[0].text));
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
    let next_char = text[rows[0].text.len()..]
        .chars()
        .next()
        .expect("cut character");
    let DataToShell::Page { events: rows, .. } = &mut reply else {
        panic!("events")
    };
    rows[0].text.push(next_char);
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() > MAX_PAGE_BYTES);
    let DataToShell::Page {
        events: rows,
        before,
        ..
    } = events(&store, thread, Some(EventId(2)))
    else {
        panic!("events")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "older");
    assert!(!rows[0].cut);
    assert_eq!(before, None);
}

#[test]
fn list_and_search_budget_cursors_resume_at_the_exact_unsent_row() {
    let scratch = Scratch::new("pages-budget");
    let mut store = Store::open(&scratch.db()).expect("store");
    for _ in 0..55 {
        add(&mut store, &"é".repeat(500), "needle");
    }
    for search in [None, Some("needle")] {
        let reply = list(&store, search, None);
        assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
        let DataToShell::ThreadList {
            request,
            threads,
            next,
        } = reply
        else {
            panic!("list")
        };
        assert_eq!(request, u32::MAX);
        assert!(threads.len() > 1 && threads.len() < MAX_PAGE_ROWS);
        assert_eq!(
            next.map(|key| key.id),
            threads.last().map(|row| row.id),
            "the cut page continues after its own last row"
        );
        let last = threads.last().expect("rows").id.0;
        let DataToShell::ThreadList { threads: more, .. } = list(&store, search, next) else {
            panic!("list")
        };
        assert_eq!(more[0].id.0, last - 1);
        let mut one_more = threads;
        one_more.push(more[0].clone());
        let over = DataToShell::ThreadList {
            request,
            next: one_more.last().map(cursor),
            threads: one_more,
        };
        assert!(frame::encode(&over, Pipe::Data).expect("frame").len() > MAX_PAGE_BYTES);
    }
}

#[test]
fn event_budget_cursor_resumes_without_skipping_a_normal_event() {
    let scratch = Scratch::new("pages-event-budget");
    let mut store = Store::open(&scratch.db()).expect("store");
    let text = "event".repeat(200);
    let thread = add(&mut store, "title", &text);
    store
        .begin_turn(thread, None, RunId(2), &text)
        .expect("turn");
    let new = NewEvent {
        kind: EventKind::AssistantText,
        text: Some(&text),
        body: None,
    };
    store.append(thread, RunId(2), &[new; 53]).expect("events");
    let reply = events(&store, thread, None);
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
    let DataToShell::Page {
        request,
        events: rows,
        before,
        ..
    } = reply
    else {
        panic!("events")
    };
    assert!(rows.len() > 1 && rows.len() < MAX_PAGE_ROWS);
    assert!(rows.iter().all(|event| !event.cut && event.text == text));
    assert_eq!(before, rows.last().map(|event| event.id));
    let last = before.expect("continuation").0;
    let DataToShell::Page { events: more, .. } = events(&store, thread, before) else {
        panic!("events")
    };
    assert_eq!(more[0].id.0, last - 1);
    let mut one_more = rows;
    one_more.push(more[0].clone());
    let over = DataToShell::Page {
        request,
        thread,
        before: one_more.last().map(|event| event.id),
        events: one_more,
    };
    assert!(frame::encode(&over, Pipe::Data).expect("frame").len() > MAX_PAGE_BYTES);
}

#[test]
fn an_impossible_title_is_shortened_without_stalling_pagination() {
    let scratch = Scratch::new("pages-title");
    let mut store = Store::open(&scratch.db()).expect("store");
    add(&mut store, "older", "needle");
    let title = "界".repeat(100_000);
    add(&mut store, &title, "needle");
    for search in [None, Some("needle")] {
        let reply = list(&store, search, None);
        assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
        let DataToShell::ThreadList { threads, next, .. } = reply else {
            panic!("list")
        };
        assert_eq!(ids(&threads), vec![2]);
        assert!(!threads[0].title.is_empty());
        assert!(title.starts_with(&threads[0].title));
        assert_eq!(next.map(|key| key.id), Some(threads[0].id));
        let DataToShell::ThreadList { threads, next, .. } = list(&store, search, next) else {
            panic!("list")
        };
        assert_eq!(ids(&threads), vec![1]);
        assert_eq!(next, None);
    }
}

#[test]
fn page_reads_release_the_snapshot_before_a_passive_checkpoint() {
    let scratch = Scratch::new("pages-checkpoint");
    let mut store = Store::open(&scratch.db()).expect("store");
    store
        .conn
        .execute_batch("PRAGMA wal_autocheckpoint = 0;")
        .expect("manual checkpoints");
    let thread = add(&mut store, "title", "needle");
    let helper = rusqlite::Connection::open(scratch.db()).expect("helper");
    for page in [
        PageOf::Threads {
            search: None,
            after: None,
        },
        PageOf::Threads {
            search: Some("needle".to_owned()),
            after: None,
        },
        PageOf::Events {
            thread,
            before: None,
        },
    ] {
        store.read_page(1, page).expect("read");
        add(&mut store, "after the read", "more");
        let (busy, frames, copied): (i64, i64, i64) = helper
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("checkpoint");
        assert_eq!(busy, 0);
        assert!(frames > 0);
        assert_eq!(copied, frames);
        assert!(store.conn.is_autocommit());
    }
}

#[test]
fn a_stepped_serving_statement_still_allows_a_full_drain_and_purge() {
    let scratch = Scratch::new("pages-stepped");
    let mut store = Store::open(&scratch.db()).expect("store");
    store
        .conn
        .execute_batch("PRAGMA wal_autocheckpoint = 0;")
        .expect("manual checkpoints");
    let doomed = add(&mut store, "doomed", "removed");
    let live = add(&mut store, "live", "kept");
    store.begin_delete(doomed).expect("hide");
    store.finish_delete(doomed).expect("delete");
    let stop = Arc::new(AtomicBool::new(false));
    let copied = {
        let mut query = store
            .conn
            .prepare("SELECT id FROM threads")
            .expect("statement");
        let mut rows = query.query([]).expect("query");
        assert!(rows.next().expect("step").is_some());
        assert!(purge::drain(&scratch.db(), &stop).expect("drain"));
        let helper = rusqlite::Connection::open(scratch.db()).expect("helper");
        let (frames, copied): (i64, i64) = helper
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(1)?, row.get(2)?))
            })
            .expect("checkpoint");
        assert!(frames > 0);
        assert_eq!(copied, frames);
        store
            .owed()
            .expect("owed")
            .expect("idle")
            .copy(&stop)
            .expect("copy")
            .expect("not stopped")
    };
    assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
    store.leftovers().remove().expect("remove");
    assert!(!store.purge_pending());
    assert!(!store.leftovers().exist());
    assert_eq!(
        ids(&store.thread_page(None, 50).expect("live survives").0),
        vec![live.0]
    );
}

#[test]
fn page_failures_never_expose_sqlite_content() {
    let scratch = Scratch::new("pages-errors");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = add(&mut store, "private title", "private search");
    store
        .conn
        .execute_batch("DROP TRIGGER events_append_only;")
        .expect("test corruption");
    store
        .conn
        .execute("UPDATE events SET kind = 'private content'", [])
        .expect("bad kind");
    let error = store
        .read_page(
            1,
            PageOf::Events {
                thread,
                before: None,
            },
        )
        .expect_err("refused");
    assert_eq!(format!("{error:?}"), "Read");
    assert_eq!(error.to_string(), "can't read the store page");
    store
        .conn
        .execute_batch("ALTER TABLE threads RENAME TO unavailable;")
        .expect("test failure");
    for error in [
        store.thread_page(None, 50).expect_err("list fails"),
        store
            .search_page("private search", 50)
            .expect_err("search fails"),
    ] {
        assert_eq!(format!("{error:?}"), "Read");
        assert_eq!(error.to_string(), "can't read the store page");
        assert!(std::error::Error::source(&error).is_none());
    }
}
