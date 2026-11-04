//! The builder over a real store: one read transaction, origin-restricted references, whole
//! runs under an exact bound, and nothing raw in `Debug`.

use kumochi_protocol::message::{
    AssistantName, Effort, EventId, ModelChoice, ModelSlug, RunId, ThreadId, UserName,
};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::bound::{item_bytes, request_bytes};
use super::build::{Turn, build};
use super::*;
use crate::engines::{CallId, Opaque};
use crate::store::context::Snapshot;
use crate::store::{EventKind, NewEvent, RunEnd, Store};
use crate::test_support::Scratch;

const TIME: &str = "Tuesday 4 November 2026, 09:30";

struct World {
    scratch: Scratch,
    store: Store,
    main: ThreadId,
}

fn world(name: &str) -> World {
    let scratch = Scratch::new(name);
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    let main = store.main_thread().expect("main");
    World {
        scratch,
        store,
        main,
    }
}

impl World {
    /// A finished run: the user's `ask` and, when given, the assistant's `reply`. `None` starts a
    /// new task.
    fn run(
        &mut self,
        thread: Option<ThreadId>,
        ask: &str,
        reply: Option<&str>,
    ) -> (ThreadId, RunId) {
        let (high_thread, high_run) = self.store.high_water().expect("ids");
        let run = RunId(high_run.0 + 1);
        let (thread, title) = match thread {
            Some(thread) => (thread, None),
            None => (ThreadId(high_thread.0 + 1), Some("Clouds deck")),
        };
        self.store
            .begin_turn(thread, title, run, ask)
            .expect("turn");
        let events: Vec<NewEvent<'_>> = reply
            .iter()
            .map(|reply| NewEvent {
                kind: EventKind::AssistantText,
                text: Some(reply),
                body: None,
            })
            .collect();
        self.store
            .end_run(thread, run, &events, RunEnd::Done)
            .expect("ended");
        (thread, run)
    }

    /// The events of `thread`, oldest first.
    fn events(&self, thread: ThreadId) -> Vec<EventId> {
        let conn = self.store.conn_for_tests();
        let mut statement = conn
            .prepare("SELECT id FROM events WHERE thread = ?1 ORDER BY id")
            .expect("prepared");
        statement
            .query_map([i64::try_from(thread.0).expect("small")], |row| {
                row.get::<_, i64>(0)
            })
            .expect("rows")
            .map(|id| EventId(u64::try_from(id.expect("id")).expect("positive")))
            .collect()
    }

    fn build(
        &self,
        run: &RunSnapshot,
        turn: (ThreadId, RunId),
        supplied: Supplied,
        bound: usize,
    ) -> Result<Built, BuildError> {
        let reader = Connection::open(self.scratch.db()).expect("reader");
        let turn = Turn {
            thread: turn.0,
            run: turn.1,
            local_time: TIME,
        };
        build(
            Snapshot::begin(&reader).expect("snapshot"),
            run,
            turn,
            supplied,
            bound,
        )
    }
}

fn snapshot(origin: Origin, user: Option<&str>) -> RunSnapshot {
    RunSnapshot {
        origin,
        assistant: AssistantName::new("Kumo").expect("name"),
        user: user.map(|user| UserName::new(user).expect("name")),
        choice: ModelChoice {
            model: ModelSlug::new("gpt-6-luna").expect("slug"),
            effort: Effort::Medium,
        },
        connection: ConnectionTag(7),
    }
}

fn call(id: &str) -> CallId {
    CallId::new(id).expect("call id")
}

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

/// A Kumo run whose history call `call` showed `refs`, placed after every stored event.
fn history(run: RunId, after: EventId, refs: Vec<HistoryRef>) -> Vec<Item> {
    vec![
        Item {
            id: EventId(after.0 + 1),
            run,
            body: Body::Call {
                call: call("c1"),
                name: "read_history".to_owned(),
                arguments: "{}".to_owned(),
            },
        },
        Item {
            id: EventId(after.0 + 2),
            run,
            body: Body::History {
                call: call("c1"),
                refs,
            },
        },
    ]
}

/// The quoted entries of the request's only tool result.
fn quoted(built: &Built) -> Vec<Value> {
    let outputs: Vec<&String> = built
        .request
        .input
        .iter()
        .filter_map(|item| match item {
            InputItem::FunctionOutput { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 1, "one history result");
    let value: Value = serde_json::from_str(outputs[0]).expect("json");
    value["quoted_history"].as_array().expect("a list").clone()
}

fn whole(range: &[EventId], thread: ThreadId) -> HistoryRef {
    HistoryRef {
        thread,
        first: range[0],
        last: range[range.len() - 1],
    }
}

#[test]
fn a_kumo_request_states_its_model_names_role_and_ordered_runs() {
    let mut world = world("ctx-basic");
    let main = world.main;
    world.run(Some(main), "first ask", Some("first reply"));
    let (_, current) = world.run(Some(main), "second ask", None);
    let built = world
        .build(
            &snapshot(Origin::Kumo, Some("Oscar")),
            (main, current),
            Supplied::default(),
            BOUND,
        )
        .expect("built");
    assert_eq!(built.request.model.as_str(), "gpt-6-luna");
    assert_eq!(built.request.effort, Effort::Medium);
    assert_eq!(
        built.request.input,
        [
            message(Role::User, "first ask"),
            message(Role::Assistant, "first reply"),
            message(Role::User, &format!("second ask\n\nLocal time: {TIME}")),
        ]
    );
    assert_eq!(built.request.tools, []);
    assert_eq!(built.sources, [main]);
    let instructions = &built.request.instructions;
    assert!(
        instructions.starts_with("You are Kumo, the assistant inside Kumochi, a desktop app"),
        "names: {instructions}"
    );
    assert!(
        instructions.contains("\nThe user's name is Oscar.\n"),
        "user line"
    );
    assert!(
        instructions.contains("This is your main conversation"),
        "Kumo's role"
    );
    assert!(!instructions.contains('\r'), "one line ending");
}

#[test]
fn an_unset_user_name_adds_nothing_and_a_name_is_never_read_as_a_placeholder() {
    let mut world = world("ctx-names");
    let main = world.main;
    let (_, current) = world.run(Some(main), "ask", None);
    let mut run = snapshot(Origin::Kumo, None);
    let unnamed = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("built");
    assert!(
        !unnamed.request.instructions.contains("user's name"),
        "no user line"
    );
    assert!(
        !unnamed.request.instructions.contains('{'),
        "every placeholder filled"
    );
    run.assistant = AssistantName::new("{product} {user}").expect("name");
    let odd = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("built");
    assert!(
        odd.request
            .instructions
            .starts_with("You are {product} {user}, the assistant inside Kumochi"),
        "kept as written"
    );
}

#[test]
fn a_task_gets_the_task_role_and_a_mismatched_origin_builds_nothing() {
    let mut world = world("ctx-origin");
    let main = world.main;
    let (task, current) = world.run(None, "task ask", None);
    let built = world
        .build(
            &snapshot(Origin::Task(task), None),
            (task, current),
            Supplied::default(),
            BOUND,
        )
        .expect("built");
    assert!(
        built
            .request
            .instructions
            .contains("This conversation is a task"),
        "task role"
    );
    let (_, kumo_run) = world.run(Some(main), "ask", None);
    for (origin, turn) in [
        (Origin::Kumo, (task, current)),
        (Origin::Task(task), (main, kumo_run)),
        (Origin::Task(main), (main, kumo_run)),
        (Origin::Task(ThreadId(task.0 + 1)), (task, current)),
    ] {
        let refused = world.build(&snapshot(origin, None), turn, Supplied::default(), BOUND);
        assert!(
            matches!(refused, Err(BuildError::Origin)),
            "{origin:?}: {refused:?}"
        );
    }
}

#[test]
fn a_thread_being_deleted_or_gone_builds_nothing_and_a_missing_turn_is_refused() {
    let mut world = world("ctx-deleting");
    let (hidden, hidden_run) = world.run(None, "a", None);
    let (gone, gone_run) = world.run(None, "b", None);
    world.store.begin_delete(hidden).expect("hidden");
    world.store.delete(gone);
    for (thread, run) in [(hidden, hidden_run), (gone, gone_run)] {
        let built = world.build(
            &snapshot(Origin::Task(thread), None),
            (thread, run),
            Supplied::default(),
            BOUND,
        );
        assert!(matches!(built, Err(BuildError::Deleting)), "{built:?}");
    }
    let (task, run) = world.run(None, "c", None);
    let missing = world.build(
        &snapshot(Origin::Task(task), None),
        (task, RunId(run.0 + 1)),
        Supplied::default(),
        BOUND,
    );
    assert!(matches!(missing, Err(BuildError::NoTurn)), "{missing:?}");
}

#[test]
fn kumo_re_reads_each_reference_and_reports_exactly_the_sources_it_rendered() {
    let mut world = world("ctx-refs");
    let main = world.main;
    let (b, _) = world.run(None, "b ask", Some("b reply"));
    let (c, _) = world.run(None, "c ask", None);
    let (deleted, _) = world.run(None, "gone text", None);
    let (deleting, _) = world.run(None, "hidden text", None);
    let (b_ids, c_ids) = (world.events(b), world.events(c));
    let (gone_ids, hidden_ids) = (world.events(deleted), world.events(deleting));
    world.store.delete(deleted);
    world.store.begin_delete(deleting).expect("hidden");
    let (_, current) = world.run(Some(main), "what happened?", None);
    let last = *world.events(main).last().expect("an event");
    let refs = vec![
        whole(&b_ids, b),
        whole(&c_ids, c),
        whole(&gone_ids, deleted),
        whole(&hidden_ids, deleting),
        HistoryRef {
            thread: b,
            first: EventId(b_ids[1].0 + 1),
            last: EventId(b_ids[1].0 + 1),
        },
    ];
    let supplied = Supplied {
        summary: None,
        items: history(current, last, refs),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        quoted(&built),
        [
            json!({ "task": b.0, "title": "Clouds deck", "events": [b_ids[0].0, b_ids[1].0],
                    "text": "user: b ask\nassistant: b reply", "truncated": false,
                    "continue_from": null }),
            json!({ "task": c.0, "title": "Clouds deck", "events": [c_ids[0].0, c_ids[0].0],
                    "text": "user: c ask", "truncated": false, "continue_from": null }),
            json!({ "task": deleted.0, "note": REMOVED }),
            json!({ "task": deleting.0, "note": REMOVED }),
            json!({ "task": b.0, "title": "Clouds deck", "events": null, "text": "",
                    "truncated": false, "continue_from": null }),
        ]
    );
    assert_eq!(built.sources, [main, b, c]);
}

#[test]
fn a_task_never_dereferences_another_tasks_reference_and_kumo_never_its_own() {
    let mut world = world("ctx-foreign");
    let main = world.main;
    let (b, _) = world.run(None, "secret of b", None);
    let b_ids = world.events(b);
    let (c, current) = world.run(None, "c ask", None);
    let last = *world.events(c).last().expect("an event");
    let supplied = Supplied {
        summary: None,
        items: history(current, last, vec![whole(&b_ids, b)]),
    };
    let built = world
        .build(
            &snapshot(Origin::Task(c), None),
            (c, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        quoted(&built),
        [json!({ "task": b.0, "note": UNAVAILABLE })]
    );
    assert!(
        !format!("{:?}", built.request.input).contains("secret"),
        "debug"
    );
    assert!(
        built.request.input.iter().all(|item| match item {
            InputItem::Message { text, .. } | InputItem::FunctionOutput { output: text, .. } =>
                !text.contains("secret of b"),
            _ => true,
        }),
        "B's text never appears"
    );
    assert_eq!(built.sources, [c]);

    let main_ids = world.events(main);
    let (_, kumo_run) = world.run(Some(main), "ask", None);
    let last = *world.events(main).last().expect("an event");
    let main_ref = HistoryRef {
        thread: main,
        first: EventId(1),
        last,
    };
    assert!(main_ids.is_empty(), "Kumo's thread had no earlier events");
    let supplied = Supplied {
        summary: None,
        items: history(kumo_run, last, vec![main_ref]),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, kumo_run),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        quoted(&built),
        [json!({ "task": main.0, "note": UNAVAILABLE })]
    );
}

#[test]
fn the_conversation_and_its_sources_come_from_one_snapshot() {
    let mut world = world("ctx-snapshot");
    let (b, _) = world.run(None, "before", None);
    let b_ids = world.events(b);
    let reader = Connection::open(world.scratch.db()).expect("reader");
    let snapshot = Snapshot::begin(&reader).expect("snapshot");
    world.run(Some(b), "after", None);
    world.store.delete(b);
    let source = snapshot.source(b).expect("read").expect("still there");
    assert!(source.live, "the delete committed after the snapshot");
    let excerpt = snapshot
        .excerpt(b, b_ids[0], EventId(u64::MAX >> 1), REREAD_CAP)
        .expect("read");
    assert_eq!(excerpt.text, "user: before");
    snapshot.end().expect("ended");
    let fresh = Snapshot::begin(&reader).expect("snapshot");
    assert_eq!(fresh.source(b).expect("read"), None);
}

#[test]
fn an_excerpt_keeps_whole_lines_up_to_its_cap_and_cuts_one_oversized_event_on_a_boundary() {
    let mut world = world("ctx-cap");
    let (b, _) = world.run(None, "hi", Some("yo"));
    let (e, _) = world.run(None, "ééé", None);
    world.run(Some(e), "next", None);
    let (b_ids, e_ids) = (world.events(b), world.events(e));
    let reader = Connection::open(world.scratch.db()).expect("reader");
    let snapshot = Snapshot::begin(&reader).expect("snapshot");
    let both = "user: hi\nassistant: yo";
    let exact = snapshot
        .excerpt(b, b_ids[0], b_ids[1], both.len())
        .expect("read");
    assert_eq!(
        (exact.text.as_str(), exact.shown, exact.cut, exact.more),
        (both, Some((b_ids[0], b_ids[1])), false, None)
    );
    let short = snapshot
        .excerpt(b, b_ids[0], b_ids[1], both.len() - 1)
        .expect("read");
    assert_eq!(
        (short.text.as_str(), short.shown, short.cut, short.more),
        (
            "user: hi",
            Some((b_ids[0], b_ids[0])),
            false,
            Some(b_ids[1])
        )
    );
    // "user: ééé" is 12 bytes; 9 falls inside the second é.
    let cut = snapshot.excerpt(e, e_ids[0], e_ids[1], 9).expect("read");
    assert_eq!(
        (cut.text.as_str(), cut.shown, cut.cut, cut.more),
        ("user: é", Some((e_ids[0], e_ids[0])), true, Some(e_ids[1]))
    );
    let empty = snapshot
        .excerpt(e, e_ids[1], e_ids[0], REREAD_CAP)
        .expect("read");
    assert_eq!(empty, crate::store::context::Excerpt::default());
}

#[test]
fn a_split_or_repeated_tool_group_is_a_typed_error() {
    let mut world = world("ctx-groups");
    let main = world.main;
    let (_, current) = world.run(Some(main), "ask", None);
    let last = *world.events(main).last().expect("an event");
    let pair = history(current, last, Vec::new());
    let cases = [
        vec![pair[0].clone()],
        vec![pair[1].clone()],
        vec![
            pair[1].clone(),
            Item {
                id: EventId(last.0 + 3),
                ..pair[0].clone()
            },
        ],
        vec![
            Item {
                id: last,
                ..pair[0].clone()
            },
            pair[1].clone(),
        ],
        vec![pair[0].clone(), pair[0].clone(), pair[1].clone()],
    ];
    for items in cases {
        let built = world.build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            Supplied {
                summary: None,
                items,
            },
            BOUND,
        );
        assert!(matches!(built, Err(BuildError::InvalidCut)), "{built:?}");
    }
}

#[test]
fn reasoning_is_replayed_only_on_the_connection_that_made_it() {
    let mut world = world("ctx-reasoning");
    let main = world.main;
    let (_, current) = world.run(Some(main), "ask", None);
    let last = *world.events(main).last().expect("an event");
    let item = |id: u64, tag: u64, byte: u8| Item {
        id: EventId(last.0 + id),
        run: current,
        body: Body::Reasoning {
            connection: ConnectionTag(tag),
            item: Opaque::new(vec![byte]).expect("small"),
        },
    };
    let supplied = Supplied {
        summary: None,
        items: vec![item(1, 7, 1), item(2, 8, 2)],
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        built.request.input[1..],
        [InputItem::Reasoning(Opaque::new(vec![1]).expect("small"))]
    );
}

#[test]
fn a_summary_states_its_range_and_replaces_the_events_it_covers() {
    let mut world = world("ctx-summary");
    let main = world.main;
    world.run(Some(main), "old ask", Some("old reply"));
    let covered = world.events(main);
    let (_, current) = world.run(Some(main), "new ask", None);
    let supplied = Supplied {
        summary: Some(Summary {
            first: covered[0],
            last: covered[1],
            text: "They talked about clouds.".to_owned(),
        }),
        items: Vec::new(),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        built.request.input,
        [
            message(
                Role::Developer,
                &format!(
                    "Summary of events {}–{}:\nThey talked about clouds.",
                    covered[0].0, covered[1].0
                )
            ),
            message(Role::User, &format!("new ask\n\nLocal time: {TIME}")),
        ]
    );
}

#[test]
fn the_bound_is_exact_and_older_runs_go_first() {
    let mut world = world("ctx-bound");
    let main = world.main;
    world.run(Some(main), "oldest", Some("x".repeat(1000).as_str()));
    world.run(Some(main), "middle", None);
    let (_, current) = world.run(Some(main), "current", None);
    let run = snapshot(Origin::Kumo, None);
    let all = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("built");
    let size = request_bytes(&all.request);
    let exact = world
        .build(&run, (main, current), Supplied::default(), size)
        .expect("fits exactly");
    assert_eq!(exact, all);
    let over = world
        .build(&run, (main, current), Supplied::default(), size - 1)
        .expect("built");
    assert_eq!(
        over.request.input,
        [
            message(Role::User, "middle"),
            message(Role::User, &format!("current\n\nLocal time: {TIME}")),
        ]
    );
    let alone = request_bytes(&ModelRequest {
        input: over.request.input[1..].to_vec(),
        ..over.request.clone()
    });
    let only = world
        .build(&run, (main, current), Supplied::default(), alone)
        .expect("the current run alone fits");
    assert_eq!(
        only.request.input,
        [message(
            Role::User,
            &format!("current\n\nLocal time: {TIME}")
        )]
    );
    let too_large = world.build(&run, (main, current), Supplied::default(), alone - 1);
    assert!(
        matches!(too_large, Err(BuildError::TooLarge)),
        "{too_large:?}"
    );
}

#[test]
fn an_oversized_summary_or_turn_ends_with_too_large() {
    let mut world = world("ctx-oversized");
    let main = world.main;
    let huge = "y".repeat(BOUND);
    let (_, current) = world.run(Some(main), &huge, None);
    let run = snapshot(Origin::Kumo, None);
    let turn = world.build(&run, (main, current), Supplied::default(), BOUND);
    assert!(matches!(turn, Err(BuildError::TooLarge)), "{turn:?}");
    let (_, small) = world.run(Some(main), "small", None);
    let summary = Supplied {
        summary: Some(Summary {
            first: EventId(1),
            last: *world.events(main).first().expect("an event"),
            text: huge,
        }),
        items: Vec::new(),
    };
    let summarised = world.build(&run, (main, small), summary, BOUND);
    assert!(
        matches!(summarised, Err(BuildError::TooLarge)),
        "{summarised:?}"
    );
}

#[test]
fn at_most_the_newest_eight_runs_stay_raw_and_a_dropped_runs_sources_dont_count() {
    let mut world = world("ctx-runs");
    let main = world.main;
    let (b, _) = world.run(None, "b text", None);
    let b_ids = world.events(b);
    let (_, first) = world.run(Some(main), "ask 0", None);
    let first_event = *world.events(main).last().expect("an event");
    // A gap of b's events keeps the first run's history items before the next run's.
    world.run(Some(b), "gap", None);
    world.run(Some(b), "gap", None);
    let mut current = first;
    for n in 1..=8 {
        current = world.run(Some(main), &format!("ask {n}"), None).1;
    }
    let supplied = Supplied {
        summary: None,
        items: history(first, first_event, vec![whole(&b_ids, b)]),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    let asks: Vec<&str> = built
        .request
        .input
        .iter()
        .map(|item| match item {
            InputItem::Message { text, .. } => text.as_str(),
            _ => "tool",
        })
        .collect();
    assert_eq!(asks.len(), RAW_RUNS);
    assert_eq!(asks[0], "ask 1");
    assert_eq!(built.sources, [main]);
}

#[test]
fn a_title_shown_from_an_empty_range_still_counts_its_task_as_a_source() {
    let mut world = world("ctx-title-source");
    let main = world.main;
    let (high_thread, high_run) = world.store.high_water().expect("ids");
    let b = ThreadId(high_thread.0 + 1);
    world
        .store
        .begin_turn(
            b,
            Some("plan the surprise party"),
            RunId(high_run.0 + 1),
            "plan the surprise party",
        )
        .expect("turn");
    let b_ids = world.events(b);
    let (_, current) = world.run(Some(main), "what about b?", None);
    let last = *world.events(main).last().expect("an event");
    let empty = HistoryRef {
        thread: b,
        first: EventId(b_ids[0].0 + 1),
        last: EventId(b_ids[0].0 + 1),
    };
    let supplied = Supplied {
        summary: None,
        items: history(current, last, vec![empty]),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(
        quoted(&built),
        [
            json!({ "task": b.0, "title": "plan the surprise party", "events": null, "text": "",
                 "truncated": false, "continue_from": null })
        ]
    );
    assert_eq!(built.sources, [main, b]);
}

#[test]
fn a_kept_older_runs_history_counts_its_sources() {
    let mut world = world("ctx-older-sources");
    let main = world.main;
    let (b, _) = world.run(None, "b text", None);
    let b_ids = world.events(b);
    let (_, older) = world.run(Some(main), "older ask", None);
    let older_event = *world.events(main).last().expect("an event");
    world.run(Some(b), "gap", None);
    world.run(Some(b), "gap", None);
    let (_, current) = world.run(Some(main), "current ask", None);
    let supplied = Supplied {
        summary: None,
        items: history(older, older_event, vec![whole(&b_ids, b)]),
    };
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            supplied,
            BOUND,
        )
        .expect("built");
    assert_eq!(built.request.input.len(), 4, "the older run is kept whole");
    assert_eq!(built.sources, [main, b]);
}

#[test]
fn an_items_size_counts_its_escaped_call_id_and_text() {
    // 64 for the item's fields; "a\"b" escapes to 6 bytes with its quotes, and "x\ny" to 6.
    let small = InputItem::FunctionOutput {
        call: call("a\"b"),
        output: "x\ny".to_owned(),
    };
    assert_eq!(item_bytes(&small), 64 + 6 + 6);
    // Each NUL escapes to the six bytes \u0000: 64 of them and the quotes are 386 bytes.
    let nuls = InputItem::FunctionCall {
        call: call(&"\0".repeat(64)),
        name: "n".to_owned(),
        arguments: String::new(),
    };
    assert_eq!(item_bytes(&nuls), 64 + 386 + 3 + 2);
}

#[test]
fn a_bound_one_byte_short_drops_an_older_tool_pair_whole() {
    let mut world = world("ctx-pair-cut");
    let main = world.main;
    let (high_thread, high_run) = world.store.high_water().expect("ids");
    let b = ThreadId(high_thread.0 + 1);
    world
        .store
        .begin_turn(b, Some("b"), RunId(high_run.0 + 1), "b text")
        .expect("turn");
    let b_ids = world.events(b);
    let (_, older) = world.run(Some(main), "older ask", None);
    let older_event = *world.events(main).last().expect("an event");
    world.run(Some(b), "gap", None);
    world.run(Some(b), "gap", None);
    let (_, current) = world.run(Some(main), "current ask", None);
    let supplied = || Supplied {
        summary: None,
        items: history(older, older_event, vec![whole(&b_ids[..1], b)]),
    };
    let run = snapshot(Origin::Kumo, None);
    let output = json!({ "quoted_history": [{ "task": b.0, "title": "b",
        "events": [b_ids[0].0, b_ids[0].0], "text": "user: b text", "truncated": false,
        "continue_from": null }] })
    .to_string();
    let now = message(Role::User, &format!("current ask\n\nLocal time: {TIME}"));
    let full = vec![
        message(Role::User, "older ask"),
        InputItem::FunctionCall {
            call: call("c1"),
            name: "read_history".to_owned(),
            arguments: "{}".to_owned(),
        },
        InputItem::FunctionOutput {
            call: call("c1"),
            output,
        },
        now.clone(),
    ];
    let fits = world
        .build(&run, (main, current), supplied(), BOUND)
        .expect("built");
    let size = request_bytes(&fits.request);
    let exact = world
        .build(&run, (main, current), supplied(), size)
        .expect("fits exactly");
    assert_eq!(exact.request.input, full);
    assert_eq!(exact.sources, [main, b]);
    let short = world
        .build(&run, (main, current), supplied(), size - 1)
        .expect("built");
    assert_eq!(short.request.input, [now]);
    assert_eq!(short.sources, [main]);
}

#[test]
fn debug_and_errors_carry_no_content() {
    let mut world = world("ctx-debug");
    let main = world.main;
    world.run(Some(main), "private words", Some("private reply"));
    let (_, current) = world.run(Some(main), "more private words", None);
    let built = world
        .build(
            &snapshot(Origin::Kumo, Some("Oscar")),
            (main, current),
            Supplied::default(),
            BOUND,
        )
        .expect("built");
    let shown = format!("{built:?}");
    assert!(
        !shown.contains("private") && !shown.contains("Oscar"),
        "{shown}"
    );
}
