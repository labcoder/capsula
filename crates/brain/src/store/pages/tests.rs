use std::sync::Arc;

use super::*;
use crate::drawing::{Reply, Target, Write};
use crate::store::{Cancel, EventKind, NewEvent, RunEnd, Store, Swap, purge};
use crate::test_support::Scratch;

#[test]
fn issue_metadata_is_closed_positive_and_bounded_before_json_decode() {
    use kumochi_protocol::message::DraftId;
    assert_eq!(issue_draft(Some(br#"{"draft":1}"#)).unwrap(), DraftId(1));
    assert_eq!(
        issue_draft(Some(br#"{"draft":9223372036854775807}"#)).unwrap(),
        DraftId(9_223_372_036_854_775_807)
    );
    let mut boundary = br#"{"draft":7}"#.to_vec();
    boundary.resize(32, b' ');
    assert_eq!(issue_draft(Some(&boundary)).unwrap(), DraftId(7));
    boundary.push(b' ');
    assert!(matches!(
        issue_draft(Some(&boundary)),
        Err(rusqlite::Error::InvalidQuery)
    ));
    for bad in [
        b"".as_slice(),
        br"{}".as_slice(),
        br#"{"draft":0}"#,
        br#"{"draft":-1}"#,
        br#"{"draft":1.0}"#,
        br#"{"draft":"1"}"#,
        br#"{"draft":1,"draft":2}"#,
        br#"{"draft":1,"extra":0}"#,
        br#"{"draft":18446744073709551615}"#,
        br#"{"draft":1"#,
        b"\xff",
    ] {
        assert!(matches!(
            issue_draft(Some(bad)),
            Err(rusqlite::Error::InvalidQuery)
        ));
    }
    assert!(matches!(
        issue_draft(None),
        Err(rusqlite::Error::InvalidQuery)
    ));
}

#[test]
fn issue_pages_expose_only_titles_and_reject_oversized_stored_metadata() {
    use crate::store::actions_tests::{body, opened, thread, title};
    let scratch = Scratch::new("issue-event-page");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let draft = store
        .create_draft(
            owner,
            &title("visible title"),
            &body("private body marker"),
            None,
            None,
        )
        .unwrap();
    let DataToShell::Page { events, .. } = events(&store, owner, None) else {
        panic!("events");
    };
    assert_eq!(events[0].kind, PageEventKind::Issue { draft: draft.draft });
    assert_eq!(events[0].text, "visible title");
    assert!(!format!("{events:?}").contains("private body marker"));
    store.conn.execute("INSERT INTO events(thread,kind,text,body,created_ms) VALUES (?1,'action','title',?2,1)", params![bounded_id(owner.0), format!("{{\"draft\":1}}{}", " ".repeat(100_000))]).unwrap();
    assert!(matches!(
        store.pages().events_page(owner, None, 10),
        Err(StoreError::Read)
    ));
}

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
        .pages()
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
        .pages()
        .read_page(u32::MAX, PageOf::Events { thread, before })
        .expect("page")
}

fn task_rows(store: &Store, search: Option<&str>, now: i64) -> Vec<ThreadRow> {
    let page = PageOf::Threads {
        search: search.map(str::to_owned),
        after: None,
    };
    let DataToShell::ThreadList {
        threads,
        next: None,
        ..
    } = store.pages().read_page_with_clock(1, page, || now).unwrap()
    else {
        panic!("one task list page");
    };
    threads
}

#[test]
fn task_age_uses_a_strict_five_day_boundary_and_keeps_saved_outcomes() {
    use TaskRunOutcome::{Active, Done, Failed, Interrupted};

    let activity = 400;
    let five_days = 432_000_000;
    for (outcome, expected) in [
        (None, TaskState::Earlier),
        (Some(Active), TaskState::Earlier),
        (Some(Done), TaskState::Done),
        (Some(Failed), TaskState::NeedsYou),
        (Some(Interrupted), TaskState::NeedsYou),
    ] {
        for elapsed in [five_days - 1, five_days, -1] {
            assert_eq!(task_state(activity + elapsed, activity, outcome), expected);
        }
        assert_eq!(
            task_state(activity + five_days + 1, activity, outcome),
            TaskState::Sleeping
        );
    }
    assert_eq!(task_state(i64::MIN, i64::MAX, Some(Done)), TaskState::Done);
    assert_eq!(
        task_state(i64::MAX, i64::MIN, Some(Failed)),
        TaskState::Sleeping
    );
}

#[test]
fn a_task_without_a_saved_run_reads_as_earlier_on_a_query_only_connection() {
    let scratch = Scratch::new("task-before-first-run");
    let store = crate::store::actions_tests::opened(&scratch);
    store
        .conn
        .execute_batch(
            "INSERT INTO threads (title, created_ms, updated_ms) VALUES ('task', 100, 100);
         PRAGMA query_only = ON;",
        )
        .unwrap();
    let rows = task_rows(&store, None, 101);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[1].task,
        Some(TaskFact {
            latest_run: None,
            last_activity_ms: 100,
            state: TaskState::Earlier,
        })
    );
}

#[test]
fn task_pages_project_the_latest_saved_run_of_each_live_task() {
    use crate::store::actions_tests::opened;

    let scratch = Scratch::new("task-list-facts");
    let mut store = opened(&scratch);
    let main = store.main_thread().unwrap();
    let task = add(&mut store, "found", "look here");
    let older = store.high_water().unwrap().1;
    let unrelated = add(&mut store, "unrelated", "different text");
    let unrelated_run = store.high_water().unwrap().1;
    let hidden = add(&mut store, "hidden", "look here");
    let newer = RunId(store.high_water().unwrap().1.0 + 1);
    store
        .begin_turn(task, None, newer, "look here again")
        .unwrap();
    store.end_run(task, newer, &[], RunEnd::Done).unwrap();
    store
        .conn
        .execute(
            "UPDATE runs SET started_ms = 300, ended_ms = 305 WHERE id = ?1",
            [bounded_id(older.0)],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE runs SET started_ms = 100, ended_ms = 110 WHERE id = ?1",
            [bounded_id(newer.0)],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = 105 WHERE id = ?1",
            [bounded_id(task.0)],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET state = 'deleting' WHERE id = ?1",
            [bounded_id(hidden.0)],
        )
        .unwrap();

    let done = TaskFact {
        latest_run: Some(TaskRun {
            id: newer,
            outcome: TaskRunOutcome::Done,
        }),
        last_activity_ms: 110,
        state: TaskState::Done,
    };
    let plain = task_rows(&store, None, 120);
    assert_eq!(
        plain.iter().map(|row| row.id).collect::<Vec<_>>(),
        [main, task, unrelated]
    );
    assert_eq!(plain[0].task, None);
    assert_eq!(plain[1].task, Some(done));
    assert_eq!(plain[1].updated_ms, 105);
    assert_eq!(
        plain[2].task.unwrap().latest_run,
        Some(TaskRun {
            id: unrelated_run,
            outcome: TaskRunOutcome::Done,
        })
    );
    let search = task_rows(&store, Some("look here"), 121);
    assert_eq!(search, [plain[1].clone()]);
}

#[test]
fn recovery_projects_failure_without_restarting_the_task_clock() {
    use crate::store::actions_tests::opened;

    let scratch = Scratch::new("task-recovered-facts");
    let mut store = opened(&scratch);
    let thread = ThreadId(store.high_water().unwrap().0.0 + 1);
    let run = RunId(1);
    store
        .begin_turn(thread, Some("old"), run, "recover this task")
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = 100 WHERE id = ?1",
            [bounded_id(thread.0)],
        )
        .unwrap();
    store
        .conn
        .execute("UPDATE runs SET started_ms = 101 WHERE id = 1", [])
        .unwrap();
    let search = |store: &Store, now| task_rows(store, Some("recover"), now)[0].task.unwrap();
    let earlier = TaskFact {
        latest_run: Some(TaskRun {
            id: run,
            outcome: TaskRunOutcome::Active,
        }),
        last_activity_ms: 101,
        state: TaskState::Earlier,
    };
    assert_eq!(search(&store, 102), earlier);
    assert_eq!(store.recover().unwrap().crashed_runs, 1);
    let failed = TaskFact {
        latest_run: Some(TaskRun {
            id: run,
            outcome: TaskRunOutcome::Failed,
        }),
        state: TaskState::NeedsYou,
        ..earlier
    };
    assert_eq!(search(&store, 102), failed);
    let after_five_days = 432_000_102;
    assert_eq!(
        search(&store, after_five_days),
        TaskFact {
            state: TaskState::Sleeping,
            ..failed
        }
    );

    let new_run = RunId(2);
    store
        .begin_turn(thread, None, new_run, "recover and continue")
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = ?2 WHERE id = ?1",
            params![bounded_id(thread.0), after_five_days],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE runs SET started_ms = ?1 WHERE id = 2",
            [after_five_days - 1],
        )
        .unwrap();
    assert_eq!(
        search(&store, after_five_days),
        TaskFact {
            latest_run: Some(TaskRun {
                id: new_run,
                outcome: TaskRunOutcome::Active
            }),
            last_activity_ms: after_five_days,
            ..earlier
        }
    );
}

#[test]
fn saved_failure_marks_only_its_user_event_and_preserves_partial_history() {
    use crate::store::{Failure, Interruption};
    use kumochi_protocol::message::HeldScreen;

    let scratch = Scratch::new("pages-saved-failure");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("main thread");
    let thread = store.main_thread().expect("main thread");
    let completed = RunId(1);
    let typed = RunId(2);
    let held = RunId(3);
    let stopped = RunId(4);
    let crashed = RunId(5);
    for (run, screen, text, end) in [
        (completed, None, "complete", Some(RunEnd::Done)),
        (typed, None, "typed", Some(RunEnd::Failed(Failure::Engine))),
        (
            held,
            Some(HeldScreen::Screen),
            "held",
            Some(RunEnd::Failed(Failure::Engine)),
        ),
        (
            stopped,
            None,
            "stopped",
            Some(RunEnd::Interrupted(Interruption::Stop)),
        ),
        (crashed, None, "waiting", None),
    ] {
        store
            .begin_held_turn(thread, None, run, text, screen)
            .expect("saved user turn");
        if run == held {
            store.sent_screen(run).expect("saved screen metadata");
        }
        if let Some(end) = end {
            let partial = [NewEvent {
                kind: EventKind::AssistantText,
                text: Some("partial answer"),
                body: None,
            }];
            let reply = if run == typed { &partial[..] } else { &[] };
            store.end_run(thread, run, reply, end).expect("saved end");
        }
    }
    let DataToShell::Page { events: rows, .. } = events(&store, thread, None) else {
        panic!("history page");
    };
    assert_eq!(rows[0].kind, PageEventKind::User);
    drop(store);

    let mut store = Store::open(&scratch.db()).expect("reopen");
    assert_eq!(store.recover().expect("recovery").crashed_runs, 1);
    let DataToShell::Page {
        events: rows,
        next: None,
        ..
    } = events(&store, thread, None)
    else {
        panic!("one complete history page");
    };
    assert_eq!(
        rows.iter()
            .map(|event| (event.run, event.kind, event.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (Some(crashed), PageEventKind::FailedUser, "waiting"),
            (Some(stopped), PageEventKind::User, "stopped"),
            (
                Some(held),
                PageEventKind::FailedHeldTurn {
                    screen: HeldScreen::Screen,
                },
                "held",
            ),
            (Some(typed), PageEventKind::AssistantText, "partial answer"),
            (Some(typed), PageEventKind::FailedUser, "typed"),
            (Some(completed), PageEventKind::User, "complete"),
        ]
    );
}

#[test]
fn connection_failure_survives_reopen_on_its_user_event_only() {
    use kumochi_protocol::message::{
        DeliveryProof, HeldScreen, HistoryFailure, HistoryFailureCause,
    };

    let scratch = Scratch::new("connection-failure-history");
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    let thread = add(&mut store, "connection history", "older");
    let other = add(&mut store, "other task", "untouched");
    let unsent = HistoryFailure {
        cause: HistoryFailureCause::ChatGptSignIn,
        delivery: DeliveryProof::NotSent,
    };
    let rejected = HistoryFailure {
        cause: HistoryFailureCause::AnthropicKeyRejected,
        delivery: DeliveryProof::NotAccepted,
    };
    let broke_off = HistoryFailure {
        cause: HistoryFailureCause::AnthropicKeyRejected,
        delivery: DeliveryProof::PartialAnswer,
    };
    let partial = [NewEvent {
        kind: EventKind::AssistantText,
        text: Some("partial answer"),
        body: None,
    }];
    let turns = [
        (RunId(3), "typed", None, unsent),
        (RunId(4), "held", Some(HeldScreen::Screen), rejected),
        (RunId(5), "continue", None, broke_off),
    ];
    for (run, text, screen, failure) in turns {
        store
            .begin_held_turn(thread, None, run, text, screen)
            .unwrap();
        assert!(matches!(
            store.end_run_with_connection_failure(other, run, &[], failure),
            Err(StoreError::NoRun)
        ));
        if screen.is_some() {
            store.sent_screen(run).unwrap();
        }
        let final_events = if run == RunId(5) { &partial[..] } else { &[] };
        store
            .end_run_with_connection_failure(thread, run, final_events, failure)
            .unwrap();
    }
    assert!(matches!(
        store.end_run_with_connection_failure(thread, RunId(5), &partial, unsent),
        Err(StoreError::NoRun)
    ));
    store
        .conn
        .execute(
            "UPDATE runs SET started_ms = 50, ended_ms = 150 WHERE id = 5",
            [],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET updated_ms = 100 WHERE id = ?1",
            [bounded_id(thread.0)],
        )
        .unwrap();
    drop(store);

    let mut store = Store::open(&scratch.db()).unwrap();
    assert_eq!(store.recover().unwrap().crashed_runs, 0);
    let (rows, next) = store.pages().events_page(thread, None, 10).unwrap();
    assert_eq!(next, None);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].run, Some(RunId(5)));
    assert_eq!(rows[0].kind, PageEventKind::AssistantText);
    assert_eq!(rows[0].text, "partial answer");
    for (row, (run, text, screen, failure)) in rows[1..4].iter().zip(turns.into_iter().rev()) {
        assert_eq!(row.run, Some(run));
        assert_eq!(
            row.kind,
            PageEventKind::ConnectionFailed { failure, screen }
        );
        assert_eq!(row.text, text);
    }
    assert_eq!(rows[4].run, Some(RunId(1)));
    assert_eq!(rows[4].kind, PageEventKind::User);
    assert_eq!(
        task_rows(&store, Some("typed"), 151)[0].task,
        Some(TaskFact {
            latest_run: Some(TaskRun {
                id: RunId(5),
                outcome: TaskRunOutcome::Failed
            }),
            last_activity_ms: 150,
            state: TaskState::NeedsYou,
        })
    );
    let (rows, _) = store.pages().events_page(other, None, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run, Some(RunId(2)));
    assert_eq!(rows[0].kind, PageEventKind::User);
}

#[test]
fn old_unknown_and_unproven_reasons_stay_generic() {
    use crate::store::Failure;
    use kumochi_protocol::message::HeldScreen;

    let scratch = Scratch::new("generic-connection-history");
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    let thread = store.main_thread().unwrap();
    for (id, reason) in (1..).zip([
        None,
        Some("engine"),
        Some("connection/chatgpt_sign_in/unknown"),
        Some("connection/claude_code_signed_out/partial_answerX"),
        Some("connection/private_provider_text/not_sent"),
    ]) {
        let run = RunId(id);
        store
            .begin_held_turn(thread, None, run, "held", Some(HeldScreen::Screen))
            .unwrap();
        store.sent_screen(run).unwrap();
        store
            .end_run(thread, run, &[], RunEnd::Failed(Failure::Engine))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET reason = ?1 WHERE id = ?2",
                params![reason, bounded_id(id)],
            )
            .unwrap();
    }
    let reply = events(&store, thread, None);
    let encoded = frame::encode(&reply, Pipe::Data).unwrap();
    let marker = b"private_provider_text";
    assert!(!encoded.windows(marker.len()).any(|bytes| bytes == marker));
    let DataToShell::Page {
        events: rows,
        next: None,
        ..
    } = reply
    else {
        panic!("one complete history page");
    };
    assert_eq!(
        rows.into_iter().map(|row| row.kind).collect::<Vec<_>>(),
        [PageEventKind::FailedHeldTurn {
            screen: HeldScreen::Screen
        }; 5]
    );
}

#[test]
fn keyset_pages_keep_original_threads_once_while_new_threads_arrive() {
    let scratch = Scratch::new("pages-keyset");
    let mut store = Store::open(&scratch.db()).expect("store");
    for _ in 0..7 {
        add(&mut store, "title", "text");
    }
    let (first, mut next) = store.pages().thread_page(None, 2).expect("first");
    let mut seen = ids(&first);
    while next.is_some() {
        add(&mut store, "newer", "newer");
        let (rows, cursor) = store.pages().thread_page(next, 2).expect("next");
        seen.extend(ids(&rows));
        next = cursor;
    }
    assert_eq!(seen, vec![7, 6, 5, 4, 3, 2, 1]);
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 99", [])
        .expect("ties");
    let (first, next) = store.pages().thread_page(None, 2).expect("tied first");
    let (second, _) = store.pages().thread_page(next, 2).expect("tied next");
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
    let (rows, next) = store.pages().thread_page(None, 2).expect("first");
    assert_eq!(ids(&rows), vec![6, 5]);
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 100 WHERE id = 5", [])
        .expect("seen moves");
    store
        .conn
        .execute("UPDATE threads SET updated_ms = 101 WHERE id = 1", [])
        .expect("unseen moves");
    let (later, next) = store.pages().thread_page(next, 50).expect("later");
    assert_eq!(ids(&later), vec![4, 3, 2]);
    assert_eq!(next, None);
    assert_eq!(
        ids(&store.pages().thread_page(None, 2).expect("fresh").0),
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
        ids(&store.pages().thread_page(None, 50).expect("list").0),
        vec![live.0]
    );
    assert_eq!(
        ids(&store.pages().search_page("red fox", 50).expect("search").0),
        vec![live.0]
    );
    assert_eq!(
        store.pages().events_page(hidden, None, 50).expect("hidden"),
        (vec![], None)
    );
    for thread in [ThreadId(999), ThreadId(u64::MAX)] {
        assert_eq!(
            store
                .pages()
                .events_page(thread, None, 50)
                .expect("missing"),
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
    let (rows, next) = store.pages().search_page("RED fox", 1).expect("best");
    assert_eq!(ids(&rows), vec![1]);
    let (rows, next) = store
        .pages()
        .search_page_after("RED fox", next, 1)
        .expect("tie first");
    assert_eq!(ids(&rows), vec![3]);
    let (rows, next) = store
        .pages()
        .search_page_after("RED fox", next, 1)
        .expect("tie second");
    assert_eq!(ids(&rows), vec![2]);
    assert_eq!(next, None);
    for words in ["fox red", "", "\0", "red\0fox", r#"" OR * NEAR("#] {
        assert_eq!(
            store
                .pages()
                .search_page(words, 50)
                .expect("literal phrase"),
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
    let (rows, next) = store.pages().search_page("red fox", 2).expect("first");
    assert_eq!(ids(&rows), vec![1, 3]);
    store
        .begin_delete(ThreadId(3))
        .expect("hide the cursor's thread");
    let (rows, next) = store
        .pages()
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
        assert!(matches!(
            store.pages().read_page(1, page),
            Err(StoreError::Read)
        ));
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
    let (rows, next) = store.pages().search_page("needle", 50).expect("first");
    assert_eq!(ids(&rows), (6..=55).rev().collect::<Vec<_>>());
    let other = "other ".repeat(57);
    let answer = NewEvent {
        kind: EventKind::AssistantText,
        text: Some(&other),
        body: None,
    };
    store.append(thread, run, &[answer]).expect("an answer");
    let (rows, next) = store
        .pages()
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
    let (rows, before) = store.pages().events_page(thread, None, 2).expect("newest");
    assert_eq!(
        rows.iter().map(|event| event.id.0).collect::<Vec<_>>(),
        vec![4, 3]
    );
    assert_eq!(rows[0].kind, PageEventKind::SceneStep);
    assert_eq!(rows[1].kind, PageEventKind::AssistantText);
    assert_eq!(rows[0].text, "");
    assert_eq!(rows[0].run, Some(RunId(2)));
    assert_eq!(before, Some(EventId(3)));
    let (rows, before) = store.pages().events_page(thread, before, 2).expect("older");
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
fn hidden_drawing_and_tool_rows_do_not_consume_a_page_or_create_a_continuation() {
    let scratch = Scratch::new("pages-hidden-rows");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = ThreadId(1);
    store
        .begin_turn(thread, Some("fixture"), RunId(1), "oldest")
        .expect("turn");
    for kind in [
        "scene_begin",
        "scene_cleared",
        "scene_cut",
        "revealed",
        "output_item",
        "tool_result",
    ] {
        for _ in 0..MAX_PAGE_ROWS {
            store
                .conn
                .execute(
                    "INSERT INTO events(thread,run,kind,created_ms) VALUES (?1,1,?2,1)",
                    params![bounded_id(thread.0), kind],
                )
                .expect("hidden row");
        }
    }
    let (rows, before) = store
        .pages()
        .events_page(thread, None, MAX_PAGE_ROWS)
        .expect("first page");
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [EventId(1)]
    );
    assert_eq!(before, None);
    store
        .append(
            thread,
            RunId(1),
            &vec![
                NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some("fixture"),
                    body: None,
                };
                MAX_PAGE_ROWS - 1
            ],
        )
        .expect("exactly one visible page");
    let (rows, before) = store
        .pages()
        .events_page(thread, None, MAX_PAGE_ROWS)
        .expect("one page");
    assert_eq!(rows.len(), MAX_PAGE_ROWS);
    assert_eq!(rows.last().expect("oldest row").id, EventId(1));
    assert_eq!(before, None);
}

#[test]
fn oversized_unicode_event_is_cut_to_the_full_frame_budget_and_can_continue() {
    use kumochi_protocol::message::{DeliveryProof, HistoryFailure, HistoryFailureCause};

    let scratch = Scratch::new("pages-unicode");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = add(&mut store, "title", "older");
    let marker = "sensitive-text-marker";
    let text = format!("{marker}{}", "🙂漢é".repeat(40_000));
    let failure = HistoryFailure {
        cause: HistoryFailureCause::AnthropicCredit,
        delivery: DeliveryProof::NotAccepted,
    };
    store
        .begin_turn(thread, None, RunId(2), &text)
        .expect("huge event");
    store
        .end_run_with_connection_failure(thread, RunId(2), &[], failure)
        .expect("refined outcome");
    let mut reply = events(&store, thread, None);
    let debug = format!("{reply:?}");
    let marker_bytes = format!("{:?}", marker.as_bytes());
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&marker_bytes[1..marker_bytes.len() - 1]));
    let DataToShell::Page {
        request,
        events: rows,
        next: before,
        ..
    } = &reply
    else {
        panic!("events")
    };
    assert_eq!(*request, u32::MAX);
    assert_eq!(rows.len(), 1);
    assert_eq!(*before, Some(EventId(2)));
    assert_eq!(
        rows[0].kind,
        PageEventKind::ConnectionFailed {
            failure,
            screen: None
        }
    );
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
        next: before,
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
        next: before,
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
        next: one_more.last().map(|event| event.id),
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
        store.pages().read_page(1, page).expect("read");
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
    let stop = Arc::new(Cancel::default());
    let copied = {
        let mut query = store
            .conn
            .prepare("SELECT id FROM threads")
            .expect("statement");
        let mut rows = query.query([]).expect("query");
        assert!(rows.next().expect("step").is_some());
        let side = store.side();
        assert!(purge::drain(&side, side.hold_helper(), &stop).expect("drain"));
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
        ids(&store
            .pages()
            .thread_page(None, 50)
            .expect("live survives")
            .0),
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
        .pages()
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
        store.pages().thread_page(None, 50).expect_err("list fails"),
        store
            .pages()
            .search_page("private search", 50)
            .expect_err("search fails"),
    ] {
        assert_eq!(format!("{error:?}"), "Read");
        assert_eq!(error.to_string(), "can't read the store page");
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn text_past_the_budget_is_capped_in_bytes_and_keeps_its_nuls() {
    let scratch = Scratch::new("pages-nul");
    let mut store = Store::open(&scratch.db()).expect("store");
    let thread = add(&mut store, "title", "x\0y");
    let text = format!("a\0b{}", "é".repeat(20_000));
    store
        .begin_turn(thread, None, RunId(2), &text)
        .expect("huge event");
    let reply = events(&store, thread, None);
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= MAX_PAGE_BYTES);
    let DataToShell::Page {
        events: rows,
        next: before,
        ..
    } = reply
    else {
        panic!("events")
    };
    assert_eq!(rows.len(), 1);
    assert!(rows[0].cut);
    assert!(
        rows[0].text.starts_with("a\0bé"),
        "the NUL didn't end the text"
    );
    assert!(text.starts_with(&rows[0].text));
    assert_eq!(before, Some(EventId(2)));
    let DataToShell::Page { events: rows, .. } = events(&store, thread, before) else {
        panic!("events")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "x\0y");
    assert!(!rows[0].cut);
}

#[test]
fn kumos_page_shows_a_deleted_tasks_note_beside_its_messages() {
    let scratch = Scratch::new("pages-note");
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    let main = store.main_thread().expect("main");
    let doomed = add(&mut store, "doomed", "doomed ask");
    let (_, run) = store.high_water().expect("ids");
    let run = RunId(run.0 + 1);
    store.begin_turn(main, None, run, "hello").expect("turn");
    let reply = [NewEvent {
        kind: EventKind::AssistantText,
        text: Some("hi"),
        body: None,
    }];
    store.end_run(main, run, &reply, RunEnd::Done).expect("end");
    store.begin_delete(doomed).expect("hidden");
    let (rows, before) = store.pages().events_page(main, None, 50).expect("page");
    let rows: Vec<_> = rows
        .iter()
        .map(|row| (row.run, row.kind, row.text.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (None, PageEventKind::TaskDeleted, ""),
            (Some(run), PageEventKind::AssistantText, "hi"),
            (Some(run), PageEventKind::User, "hello"),
        ]
    );
    assert_eq!(before, None);
}

fn typed_scene(store: &mut Store, thread: ThreadId, run: RunId) -> SceneId {
    let header = serde_json::from_value(serde_json::json!({
        "begin": {"v":1,"scene":"history","surface":"overlay","alt":"private drawing alt"},
        "layout_version":2
    }))
    .expect("header");
    let step = Arc::new(
        serde_json::from_value(serde_json::json!({
            "id":"first","say":"private drawing text",
            "put":[{"shape":{"id":"box","kind":"rect","box":[0,0,10,10]}}]
        }))
        .expect("step"),
    );
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread,
            run,
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step,
            protected: false,
        })
        .expect("typed write")
    else {
        panic!("step reply")
    };
    scene
}

fn history_event(store: &Store, id: u64, kind: PageEventKind, text: &str) -> PageEvent {
    PageEvent {
        id: EventId(id),
        created_ms: store
            .conn
            .query_row(
                "SELECT created_ms FROM events WHERE id = ?1",
                [i64::try_from(id).expect("fixture id")],
                |row| row.get(0),
            )
            .expect("fixture timestamp"),
        run: Some(RunId(1)),
        kind,
        text: text.to_owned(),
        cut: false,
    }
}

#[test]
fn typed_history_keeps_exact_identity_neighboring_text_and_raw_cursor() {
    let scratch = Scratch::new("pages-typed-history");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("neighbor"),
                body: None,
            }],
        )
        .expect("neighbor");
    let scene = typed_scene(&mut store, thread, run);
    assert_eq!(scene, SceneId(3));
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("newer"),
                body: None,
            }; 48],
        )
        .expect("newer rows");

    let reply = events(&store, thread, None);
    let encoded = frame::encode(&reply, Pipe::Data).expect("frame");
    assert!(encoded.len() <= MAX_PAGE_BYTES);
    for private in ["private drawing alt", "private drawing text"] {
        assert!(
            !encoded
                .windows(private.len())
                .any(|bytes| bytes == private.as_bytes())
        );
    }
    let DataToShell::Page {
        request,
        thread: returned,
        events: rows,
        next: before,
    } = reply
    else {
        panic!("page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(returned, thread);
    let mut expected: Vec<_> = (5..=52)
        .rev()
        .map(|id| history_event(&store, id, PageEventKind::AssistantText, "newer"))
        .collect();
    expected.push(history_event(
        &store,
        4,
        PageEventKind::NoVisual { scene },
        "",
    ));
    expected.push(history_event(
        &store,
        2,
        PageEventKind::AssistantText,
        "neighbor",
    ));
    assert_eq!(rows, expected);
    assert_eq!(before, Some(EventId(2)));
    let DataToShell::Page {
        request,
        thread: returned,
        events: rows,
        next: before,
    } = events(&store, thread, before)
    else {
        panic!("continued page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(returned, thread);
    assert_eq!(
        rows,
        [history_event(&store, 1, PageEventKind::User, "oldest")]
    );
    assert_eq!(before, None);
}

#[test]
fn unavailable_visual_metadata_is_row_local_without_coercing_scene_identity() {
    let scratch = Scratch::new("pages-legacy-visuals");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    let scene = typed_scene(&mut store, thread, run);
    assert_eq!(scene, SceneId(2));
    let bodies = [
        None,
        Some(""),
        Some("{"),
        Some("[]"),
        Some("null"),
        Some("2"),
        Some("{}"),
        Some(r#"{"scene":null}"#),
        Some(r#"{"scene":true}"#),
        Some(r#"{"scene":false}"#),
        Some(r#"{"scene":"2"}"#),
        Some(r#"{"scene":2.0}"#),
        Some(r#"{"scene":2e0}"#),
        Some(r#"{"scene":0}"#),
        Some(r#"{"scene":-2}"#),
        Some(r#"{"scene":9223372036854775808}"#),
        Some(r#"{"scene":18446744073709551616}"#),
        Some(r#"{"scene":-9223372036854775809}"#),
        Some(r#"{"scene":1e400}"#),
        Some(r#"{"scene":[2]}"#),
        Some(r#"{"scene":{"id":2}}"#),
        Some(r#"{"scene":1}"#),
        Some(r#"{"scene":3}"#),
        Some(r#"{"scene":999}"#),
    ];
    let mut expected = vec![
        history_event(&store, 1, PageEventKind::User, "oldest"),
        history_event(&store, 3, PageEventKind::NoVisual { scene }, ""),
    ];
    for (index, body) in bodies.into_iter().enumerate() {
        store
            .append(
                thread,
                run,
                &[NewEvent {
                    kind: EventKind::SceneStep,
                    text: Some("legacy text"),
                    body,
                }],
            )
            .expect("legacy row");
        expected.push(history_event(
            &store,
            4 + u64::try_from(index).expect("index"),
            PageEventKind::SceneStep,
            "legacy text",
        ));
    }
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("newest neighbor"),
                body: Some("{"),
            }],
        )
        .expect("neighbor");
    expected.push(history_event(
        &store,
        28,
        PageEventKind::AssistantText,
        "newest neighbor",
    ));
    expected.reverse();
    let DataToShell::Page {
        request,
        thread: returned,
        events: rows,
        next: before,
    } = events(&store, thread, None)
    else {
        panic!("page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(returned, thread);
    assert_eq!(rows, expected);
    assert_eq!(before, None);
}

#[test]
fn visual_identity_requires_an_earlier_typed_begin_in_the_same_thread() {
    let scratch = Scratch::new("pages-visual-identity");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    let scene = typed_scene(&mut store, thread, run);
    assert_eq!(scene, SceneId(2));
    store
        .begin_turn(ThreadId(2), Some("other"), RunId(2), "other")
        .expect("other turn");
    assert_eq!(typed_scene(&mut store, ThreadId(2), RunId(2)), SceneId(5));
    store
        .append(
            thread,
            run,
            &[
                NewEvent {
                    kind: EventKind::SceneBegin,
                    text: None,
                    body: None,
                },
                NewEvent {
                    kind: EventKind::SceneStep,
                    text: Some("future scene"),
                    body: Some(r#"{"scene":9}"#),
                },
                NewEvent {
                    kind: EventKind::SceneBegin,
                    text: None,
                    body: Some("{}"),
                },
                NewEvent {
                    kind: EventKind::SceneStep,
                    text: Some("other thread"),
                    body: Some(r#"{"scene":5}"#),
                },
                NewEvent {
                    kind: EventKind::SceneStep,
                    text: Some("missing header"),
                    body: Some(r#"{"scene":7}"#),
                },
                NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some("neighbor"),
                    body: None,
                },
            ],
        )
        .expect("unusable references");
    let DataToShell::Page {
        events: rows,
        next: before,
        ..
    } = events(&store, thread, None)
    else {
        panic!("page")
    };
    assert_eq!(
        rows,
        [
            history_event(&store, 12, PageEventKind::AssistantText, "neighbor"),
            history_event(&store, 11, PageEventKind::SceneStep, "missing header"),
            history_event(&store, 10, PageEventKind::SceneStep, "other thread"),
            history_event(&store, 8, PageEventKind::SceneStep, "future scene"),
            history_event(&store, 3, PageEventKind::NoVisual { scene }, ""),
            history_event(&store, 1, PageEventKind::User, "oldest"),
        ]
    );
    assert_eq!(before, None);
}

#[test]
fn visual_metadata_accepts_exactly_eight_mib_but_not_one_byte_over() {
    let scratch = Scratch::new("pages-visual-body-bound");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    let scene = typed_scene(&mut store, thread, run);
    assert_eq!(scene, SceneId(2));
    let mut body = r#"{"scene":2,"padding":""#.to_owned();
    let padding = 8 * 1024 * 1024 - body.len() - 2;
    body.push_str(&"é".repeat(padding / 2));
    body.push_str("\"}");
    assert_eq!(body.len(), 8 * 1024 * 1024);
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::SceneStep,
                text: Some("at cap"),
                body: Some(&body),
            }],
        )
        .expect("at cap");
    body.push(' ');
    assert_eq!(body.len(), 8 * 1024 * 1024 + 1);
    store
        .append(
            thread,
            run,
            &[
                NewEvent {
                    kind: EventKind::SceneStep,
                    text: Some("over cap"),
                    body: Some(&body),
                },
                NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some("neighbor"),
                    body: None,
                },
            ],
        )
        .expect("over cap and neighbor");
    let DataToShell::Page {
        request,
        thread: returned,
        events: rows,
        next: before,
    } = events(&store, thread, None)
    else {
        panic!("page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(returned, thread);
    assert_eq!(
        rows,
        [
            history_event(&store, 6, PageEventKind::AssistantText, "neighbor"),
            history_event(&store, 5, PageEventKind::SceneStep, "over cap"),
            history_event(&store, 4, PageEventKind::NoVisual { scene }, "at cap"),
            history_event(&store, 3, PageEventKind::NoVisual { scene }, ""),
            history_event(&store, 1, PageEventKind::User, "oldest"),
        ]
    );
    assert_eq!(before, None);
}

#[test]
fn invalid_visual_metadata_in_the_fifty_first_row_keeps_the_page_and_cursor() {
    let scratch = Scratch::new("pages-invalid-lookahead");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::SceneStep,
                text: Some("legacy text"),
                body: Some("{"),
            }],
        )
        .expect("malformed metadata");
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("newer"),
                body: None,
            }; 50],
        )
        .expect("newer rows");
    let DataToShell::Page {
        request,
        thread: returned,
        events: rows,
        next: before,
    } = events(&store, thread, None)
    else {
        panic!("page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(returned, thread);
    assert_eq!(
        rows,
        (3..=52)
            .rev()
            .map(|id| history_event(&store, id, PageEventKind::AssistantText, "newer"))
            .collect::<Vec<_>>()
    );
    assert_eq!(before, Some(EventId(3)));
    let DataToShell::Page {
        events: rows,
        next: before,
        ..
    } = events(&store, thread, before)
    else {
        panic!("continued page")
    };
    assert_eq!(
        rows,
        [
            history_event(&store, 2, PageEventKind::SceneStep, "legacy text"),
            history_event(&store, 1, PageEventKind::User, "oldest"),
        ]
    );
    assert_eq!(before, None);
}

#[test]
fn visual_markers_participate_in_the_full_encoded_page_budget_and_continuation() {
    let scratch = Scratch::new("pages-visual-budget");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, run) = (ThreadId(1), RunId(1));
    store
        .begin_turn(thread, Some("title"), run, "oldest")
        .expect("turn");
    let base = 1_u64 << 56;
    store
        .conn
        .execute(
            "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'events'",
            [bounded_id(base - 1)],
        )
        .expect("large scene identity");
    let scene = typed_scene(&mut store, thread, run);
    assert_eq!(scene, SceneId(base));
    let body = format!(r#"{{"scene":{base}}}"#);
    let text = "event".repeat(200);
    store
        .append(
            thread,
            run,
            &[NewEvent {
                kind: EventKind::SceneStep,
                text: Some(&text),
                body: Some(&body),
            }; 53],
        )
        .expect("markers");
    let reply = events(&store, thread, None);
    assert!(frame::encode(&reply, Pipe::Data).expect("frame").len() <= 32 * 1024);
    let DataToShell::Page {
        request,
        events: rows,
        next: before,
        ..
    } = reply
    else {
        panic!("page")
    };
    assert_eq!(request, u32::MAX);
    assert_eq!(rows.len(), 31);
    assert_eq!(
        rows,
        (base + 24..=base + 54)
            .rev()
            .map(|id| history_event(&store, id, PageEventKind::NoVisual { scene }, &text))
            .collect::<Vec<_>>()
    );
    assert_eq!(before, Some(EventId(base + 24)));
    let next = events(&store, thread, before);
    assert!(
        frame::encode(&next, Pipe::Data)
            .expect("continued frame")
            .len()
            <= 32 * 1024
    );
    let DataToShell::Page {
        events: more,
        next: before,
        ..
    } = next
    else {
        panic!("continued page")
    };
    let mut expected: Vec<_> = (base + 2..=base + 23)
        .rev()
        .map(|id| history_event(&store, id, PageEventKind::NoVisual { scene }, &text))
        .collect();
    expected.push(history_event(
        &store,
        base + 1,
        PageEventKind::NoVisual { scene },
        "",
    ));
    expected.push(history_event(&store, 1, PageEventKind::User, "oldest"));
    assert_eq!(more, expected);
    assert_eq!(before, None);
    let mut one_more = rows;
    one_more.push(more[0].clone());
    let over = DataToShell::Page {
        request,
        thread,
        next: Some(EventId(base + 23)),
        events: one_more,
    };
    assert!(
        frame::encode(&over, Pipe::Data)
            .expect("one more frame")
            .len()
            > 32 * 1024
    );
}
