//! The builder over a real store: one read transaction, origin-restricted references, whole
//! runs under an exact bound, and nothing raw in `Debug`.

use kumochi_protocol::message::{
    AssistantName, Effort, EventId, ModelChoice, ModelSlug, PageEventKind, RunId, ThreadId,
    UserName,
};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::bound::{item_bytes, request_bytes};
use super::build::{Turn, build};
use super::compact::{self, Compaction, compaction};
use super::*;
use crate::engines::{CallId, Opaque};
use crate::store::context::Snapshot;
use crate::store::{EventKind, NewEvent, RunEnd, Store, StoreError};
use crate::test_support::Scratch;

const TIME: &str = "Tuesday 4 November 2026, 09:30";

mod recall_results;

struct World {
    store: Store,
    scratch: Scratch,
    main: ThreadId,
}

fn world(name: &str) -> World {
    let scratch = Scratch::new(name);
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    let main = store.main_thread().expect("main");
    World {
        store,
        scratch,
        main,
    }
}

#[test]
fn dropping_the_world_removes_its_scratch_folder() {
    let world = world("drop-world");
    let dir = world.scratch.dir().to_path_buf();
    drop(world);
    assert!(!dir.exists(), "the store closes before scratch cleanup");
}

impl World {
    fn draw(
        &mut self,
        thread: ThreadId,
        name: &str,
        alt: &str,
        title: Option<&str>,
    ) -> kumochi_protocol::message::SceneId {
        let (_, high) = self.store.high_water().expect("ids");
        let run = RunId(high.0 + 1);
        self.store
            .begin_turn(thread, None, run, "draw fixture")
            .expect("turn");
        let header = serde_json::from_value(json!({"begin":{"v":1,"scene":name,"surface":"overlay","alt":alt,"title":title},"layout_version":2})).expect("header");
        let kumochi_scene::Record::Step(step) = kumochi_scene::parse_record(
            br#"{"step":{"id":"first","put":[{"text":{"id":"x","at":[0,0],"text":"PRIVATE_STEP_CONTENT"}}]}}"#
        ).expect("step") else { panic!("step") };
        let crate::drawing::Reply::Step(scene) = self
            .store
            .drawing_write(&crate::drawing::Write::Step {
                thread,
                run,
                target: crate::drawing::Target::New(header),
                clear: Vec::new(),
                revision: 1,
                layout_version: 2,
                step: std::sync::Arc::new(step),
            })
            .expect("drawing")
        else {
            panic!("step reply")
        };
        self.store
            .end_run(thread, run, &[], RunEnd::Done)
            .expect("end");
        scene
    }

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
        choice: Some(ModelChoice {
            model: ModelSlug::new("gpt-6-luna").expect("slug"),
            effort: Effort::Medium,
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        }),
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
    assert_eq!(
        built.request.choice,
        Some(ModelChoice {
            model: ModelSlug::new("gpt-6-luna").expect("slug"),
            effort: Effort::Medium,
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        })
    );
    assert_eq!(
        built.request.input,
        [
            message(Role::User, "first ask"),
            message(Role::Assistant, "first reply"),
            message(
                Role::User,
                &format!("second ask\n\nLocal time: {TIME}\nDrawings on screen: []")
            ),
        ]
    );
    assert_eq!(
        built
            .request
            .tools
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["draw", "search_history", "read_history", "list_tasks"]
    );
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

fn drawing_line(built: &Built) -> &str {
    built
        .request
        .input
        .iter()
        .rev()
        .find_map(|item| match item {
            InputItem::Message {
                role: Role::User,
                text,
            } => text
                .split_once("\nDrawings on screen: ")
                .map(|(_, line)| line),
            _ => None,
        })
        .expect("current-turn drawing line")
}

#[test]
fn drawing_context_uses_global_membership_and_the_same_request_snapshot() {
    let mut world = world("ctx-visible-drawings");
    let main = world.main;
    world.draw(main, "displaced", "old", None);
    let (task, _) = world.run(None, "task", None);
    for n in 0..4 {
        world.draw(task, &format!("foreign{n}"), "foreign", None);
    }
    let (_, current) = world.run(Some(main), "ask", None);
    let run = snapshot(Origin::Kumo, None);
    let built = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("request");
    assert_eq!(
        drawing_line(&built),
        "[]",
        "select globally before filtering this thread"
    );
    let visible = world.draw(main, "visible", "private_header_marker", None);
    let (_, current) = world.run(Some(main), "now add", None);
    let reader = Connection::open(world.scratch.db()).expect("reader");
    let frozen = Snapshot::begin(&reader).expect("snapshot");
    world
        .store
        .clear_scenes(&[visible])
        .expect("clear after snapshot");
    let built = build(
        frozen,
        &run,
        Turn {
            thread: main,
            run: current,
            local_time: TIME,
        },
        Supplied::default(),
        BOUND,
    )
    .expect("snapshot request");
    assert_eq!(
        serde_json::from_str::<Value>(drawing_line(&built)).expect("JSON"),
        json!([{"begin":{"v":1,"scene":"visible","surface":"overlay","size":null,"title":null,"alt":"private_header_marker","others":"clear"}}])
    );
    assert!(!drawing_line(&built).contains("PRIVATE_STEP_CONTENT"));
    let after = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("fresh request");
    assert_eq!(drawing_line(&after), "[]");
    let diagnostic = format!("{built:?} {:?}", built.request);
    assert!(!diagnostic.contains("private_header_marker"));
    assert!(!diagnostic.contains(&format!("{:?}", b"private_header_marker")));
}

#[test]
fn complete_drawing_headers_count_outer_escaping_before_the_request_bound() {
    let mut world = world("ctx-drawing-bound");
    let main = world.main;
    let marker = "PRIVATE_HEADER_\"\\\n".repeat(20);
    for n in 0..4 {
        world.draw(main, &format!("scene{n}"), &marker, Some(&marker));
    }
    let (_, current) = world.run(Some(main), "ask", None);
    let run = snapshot(Origin::Kumo, None);
    let built = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("request");
    let headers: Value = serde_json::from_str(drawing_line(&built)).expect("JSON");
    assert_eq!(headers.as_array().expect("headers").len(), 4);
    for (index, header) in headers.as_array().expect("headers").iter().enumerate() {
        assert_eq!(header["begin"]["scene"], format!("scene{index}"));
        assert_eq!(header["begin"]["alt"], marker);
        assert_eq!(header["begin"]["title"], marker);
        assert_eq!(header["begin"]["others"], "clear");
    }
    let current_item = built
        .request
        .input
        .iter()
        .rev()
        .find(|item| {
            matches!(
                item,
                InputItem::Message {
                    role: Role::User,
                    ..
                }
            )
        })
        .expect("user item");
    let InputItem::Message { text, .. } = current_item else {
        panic!("message")
    };
    let escaped = serde_json::to_string(text).expect("escaped message").len();
    assert!(
        escaped > text.len(),
        "outer escaping must not be charged as bare line bytes"
    );
    let minimum =
        super::bound::fixed_bytes(&built.request.instructions, &built.request.tools) + 64 + escaped;
    let exact = world
        .build(&run, (main, current), Supplied::default(), minimum)
        .expect("exact current-item bound");
    assert_eq!(drawing_line(&exact), drawing_line(&built));
    assert!(matches!(
        world.build(&run, (main, current), Supplied::default(), minimum - 1),
        Err(BuildError::TooLarge { .. })
    ));
    let diagnostic = format!("{built:?} {:?}", built.request);
    assert!(!diagnostic.contains("PRIVATE_HEADER_"));
    assert!(!diagnostic.contains(&format!("{:?}", marker.as_bytes())));
}

#[test]
fn the_names_in_settings_win_over_names_in_earlier_turns() {
    const ASSISTANT: &str = "\nIf an earlier message or summary calls you something other than Kumo, it's out of date: Kumo is your name in the user's settings now.\n";
    const USER: &str = "\nIf an earlier message or summary calls the user something other than Oscar, it's out of date: Oscar is their name in their settings now.\n";
    let mut world = world("ctx-current-names");
    let main = world.main;
    let (_, current) = world.run(Some(main), "ask", None);
    for (user, says_user) in [(Some("Oscar"), true), (None, false)] {
        let built = world
            .build(
                &snapshot(Origin::Kumo, user),
                (main, current),
                Supplied::default(),
                BOUND,
            )
            .expect("built");
        let instructions = &built.request.instructions;
        assert!(
            instructions.contains(ASSISTANT),
            "assistant: {instructions}"
        );
        assert_eq!(
            instructions.contains(USER),
            says_user,
            "user: {instructions}"
        );
    }
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
        body: Body::Native {
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
        [InputItem::Native(Opaque::new(vec![1]).expect("small"))]
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
            message(
                Role::User,
                &format!("new ask\n\nLocal time: {TIME}\nDrawings on screen: []")
            ),
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
            message(
                Role::User,
                &format!("current\n\nLocal time: {TIME}\nDrawings on screen: []")
            ),
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
            &format!("current\n\nLocal time: {TIME}\nDrawings on screen: []")
        )]
    );
    let too_large = world.build(&run, (main, current), Supplied::default(), alone - 1);
    assert!(
        matches!(too_large, Err(BuildError::TooLarge { room: None })),
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
    assert!(
        matches!(turn, Err(BuildError::TooLarge { room: None })),
        "{turn:?}"
    );
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
        matches!(summarised, Err(BuildError::TooLarge { room: Some(_) })),
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
    let now = message(
        Role::User,
        &format!("current ask\n\nLocal time: {TIME}\nDrawings on screen: []"),
    );
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

impl World {
    /// `count` finished runs in `thread`, each an ask of `bytes` and a reply of `bytes`.
    fn runs(&mut self, thread: ThreadId, count: usize, bytes: usize) {
        for _ in 0..count {
            self.run(Some(thread), &"a".repeat(bytes), Some(&"b".repeat(bytes)));
        }
    }

    fn compact(&self, run: &RunSnapshot, thread: ThreadId, compact_at: u64) -> Option<Compaction> {
        let reader = Connection::open(self.scratch.db()).expect("reader");
        let snapshot = Snapshot::begin(&reader).expect("snapshot");
        compaction(&snapshot, run, thread, compact_at).expect("read")
    }
}

const WRITE_NOW: &str = "Write the new summary now.";

#[test]
fn compaction_waits_for_the_backlog_and_keeps_the_newest_eight_runs_raw() {
    let mut world = world("compact-threshold");
    let (task, _) = world.run(None, &"a".repeat(400), Some(&"b".repeat(400)));
    world.runs(task, 8, 400);
    let run = snapshot(Origin::Task(task), Some("Oscar"));
    // Nine runs of 800 bytes: a backlog of 1,800 estimated tokens.
    assert_eq!(world.compact(&run, task, 1801), None);
    let found = world.compact(&run, task, 1800).expect("due");
    let events = world.events(task);
    assert_eq!(found.covers, (events[0], events[1]), "only the oldest run");
    assert_eq!(found.built.sources, [task]);
    assert_eq!(
        found.built.request.input,
        [
            message(Role::User, &"a".repeat(400)),
            message(Role::Assistant, &"b".repeat(400)),
            message(Role::Developer, WRITE_NOW),
        ]
    );
    assert!(found.built.request.tools.is_empty());
    assert!(found.built.request.instructions.contains("Kumo"));
    assert!(found.built.request.instructions.contains("Oscar"));
    assert!(!found.built.request.instructions.contains('{'));
}

#[test]
fn many_small_runs_outside_the_window_reach_the_threshold() {
    let mut world = world("compact-small");
    let (task, _) = world.run(None, "aaaa", Some("bbbb"));
    world.runs(task, 19, 4);
    let run = snapshot(Origin::Task(task), None);
    // Twenty runs of 8 bytes: 40 estimated tokens, though the newest eight hold only 16.
    assert_eq!(world.compact(&run, task, 41), None);
    let found = world.compact(&run, task, 40).expect("due");
    let events = world.events(task);
    assert_eq!(
        found.covers,
        (events[0], events[23]),
        "the twelve oldest runs"
    );
}

#[test]
fn eight_runs_or_fewer_never_compact() {
    let mut world = world("compact-none");
    let (task, _) = world.run(None, &"a".repeat(10_000), None);
    world.runs(task, 7, 10_000);
    assert_eq!(
        world.compact(&snapshot(Origin::Task(task), None), task, 1),
        None
    );
}

#[test]
fn coverage_only_advances_over_whole_runs_that_fit() {
    let mut world = world("compact-oversized");
    let (task, _) = world.run(None, &"a".repeat(compact::SUMMARY_INPUT), None);
    world.runs(task, 8, 4);
    let run = snapshot(Origin::Task(task), None);
    assert_eq!(
        world.compact(&run, task, 1),
        None,
        "an oldest run over the input stalls with no request"
    );

    let mut world = self::world("compact-prefix");
    let (task, _) = world.run(None, "aaaa", Some("bbbb"));
    world.run(Some(task), &"a".repeat(compact::SUMMARY_INPUT), None);
    world.runs(task, 8, 4);
    let found = world.compact(&run, task, 1).expect("due");
    let events = world.events(task);
    assert_eq!(
        found.covers,
        (events[0], events[1]),
        "only the run that fit"
    );
}

#[test]
fn a_summary_is_carried_forward_replaces_its_events_and_never_hides_them() {
    let mut world = world("compact-summary");
    let (task, _) = world.run(None, "first zqxsummarized ask", Some("first reply"));
    world.runs(task, 8, 4);
    let events = world.events(task);
    let generation = crate::store::Generation(0);
    assert!(
        world
            .store
            .store_summary(task, generation, (events[0], events[1]), "earlier", None)
            .expect("stored")
    );
    let run = snapshot(Origin::Task(task), None);
    assert_eq!(world.compact(&run, task, 1), None, "eight uncovered runs");

    let (_, current) = world.run(Some(task), "next", None);
    let found = world.compact(&run, task, 1).expect("due");
    assert_eq!(found.covers, (events[0], events[3]));
    assert_eq!(
        found.built.request.input[0],
        message(
            Role::Developer,
            &format!(
                "Summary so far, of events {}–{}:\nearlier",
                events[0].0, events[1].0
            )
        )
    );

    let reader = Connection::open(world.scratch.db()).expect("reader");
    let read = Snapshot::begin(&reader).expect("snapshot");
    let summary = read.summary(task).expect("read");
    let excerpt = read
        .excerpt(task, events[0], events[1], REREAD_CAP)
        .expect("excerpt");
    assert!(excerpt.text.contains("zqxsummarized"), "the events stay");
    drop(read);
    let built = world
        .build(
            &run,
            (task, current),
            Supplied {
                summary,
                items: Vec::new(),
            },
            BOUND,
        )
        .expect("built");
    assert!(matches!(
        &built.request.input[0],
        InputItem::Message { role: Role::Developer, text } if text.ends_with("earlier")
    ));
    assert!(!built.request.input.iter().any(
        |item| matches!(item, InputItem::Message { text, .. } if text.contains("zqxsummarized"))
    ));

    world.store.begin_delete(task).expect("begun");
    assert_eq!(world.compact(&run, task, 1), None, "a task being deleted");
    assert!(
        !world
            .store
            .store_summary(task, generation, (events[0], events[3]), "late", None)
            .expect("ran"),
        "a delete during compaction stores nothing"
    );
}

#[test]
fn a_summary_is_trimmed_and_cut_on_a_character_boundary() {
    assert_eq!(compact::clean_summary("", compact::MAX_SUMMARY), None);
    assert_eq!(compact::clean_summary(" \n\t ", compact::MAX_SUMMARY), None);
    let exact = "s".repeat(compact::MAX_SUMMARY);
    assert_eq!(
        compact::clean_summary(&format!("  {exact}\n"), compact::MAX_SUMMARY),
        Some(exact.clone())
    );
    assert_eq!(
        compact::clean_summary(&format!("{exact}t"), compact::MAX_SUMMARY),
        Some(exact)
    );
    let multibyte = format!("{}é", "s".repeat(compact::MAX_SUMMARY - 1));
    assert_eq!(
        compact::clean_summary(&multibyte, compact::MAX_SUMMARY),
        Some("s".repeat(compact::MAX_SUMMARY - 1))
    );
}

#[test]
fn inline_compaction_rewrites_the_summary_within_the_room_the_turn_leaves() {
    let mut world = world("compact-rewrite");
    let (task, _) = world.run(None, "ask", Some("reply"));
    let run = snapshot(Origin::Task(task), None);
    let rewrite = |world: &World, room: usize| {
        let reader = Connection::open(world.scratch.db()).expect("reader");
        let read = Snapshot::begin(&reader).expect("snapshot");
        compact::rewrite(&read, &run, task, room).expect("read")
    };
    assert_eq!(rewrite(&world, 4096), None, "no summary to rewrite");
    let events = world.events(task);
    assert!(
        world
            .store
            .store_summary(
                task,
                crate::store::Generation(0),
                (events[0], events[1]),
                "long",
                None
            )
            .expect("stored")
    );
    let wrapper = item_bytes(&summary_item(events[0], events[1], ""));
    assert_eq!(rewrite(&world, wrapper), None, "no room left");
    let (found, limit) = rewrite(&world, wrapper + 100).expect("rewritten");
    assert_eq!(limit, 100);
    assert_eq!(
        found.covers,
        (events[0], events[1]),
        "it covers what the summary did"
    );
    assert_eq!(found.built.sources, [task]);
    assert_eq!(
        found.built.request.input,
        [
            message(
                Role::Developer,
                &format!(
                    "Summary so far, of events {}–{}:\nlong",
                    events[0].0, events[1].0
                )
            ),
            message(
                Role::Developer,
                "Write the new summary now, in at most 100 bytes."
            ),
        ]
    );
    let (found, _) = rewrite(&world, usize::MAX / 2).expect("rewritten");
    assert_eq!(
        found.built.request.input[1],
        message(
            Role::Developer,
            "Write the new summary now, in at most 8192 bytes."
        ),
        "the request never asks for more than the stored cap"
    );
}

#[test]
fn an_inline_answer_is_cut_where_its_escaped_text_reaches_the_allowance() {
    assert_eq!(compact::fit_summary("ab\"c", 4), Some("ab\"".to_owned()));
    assert_eq!(compact::fit_summary("\\\\\\", 3), Some("\\".to_owned()));
    assert_eq!(
        compact::fit_summary("line\nnext", 5),
        Some("line".to_owned())
    );
    assert_eq!(compact::fit_summary("é\u{1}", 7), Some("é".to_owned()));
    assert_eq!(compact::fit_summary("éé", 3), Some("é".to_owned()));
    assert_eq!(compact::fit_summary("\"", 1), None);
    assert_eq!(compact::fit_summary("  ", 10), None);
    assert_eq!(
        compact::fit_summary(&"s".repeat(compact::MAX_SUMMARY + 1), usize::MAX),
        Some("s".repeat(compact::MAX_SUMMARY)),
        "the stored cap still holds"
    );
}

#[test]
fn an_escaped_inline_rewrite_is_stored_and_the_next_build_fits() {
    let mut world = world("compact-fit");
    let (task, _) = world.run(None, "ask", Some("reply"));
    let (_, current) = world.run(Some(task), "next", None);
    let events = world.events(task);
    let run = snapshot(Origin::Task(task), None);
    let covers = (events[0], events[1]);
    assert!(
        world
            .store
            .store_summary(
                task,
                crate::store::Generation(0),
                covers,
                &"s".repeat(4000),
                None
            )
            .expect("stored")
    );
    let stored = |world: &World| {
        let reader = Connection::open(world.scratch.db()).expect("reader");
        let read = Snapshot::begin(&reader).expect("snapshot");
        Supplied {
            summary: read.summary(task).expect("read"),
            items: Vec::new(),
        }
    };
    let full = world
        .build(&run, (task, current), stored(&world), BOUND)
        .expect("built");
    let bound = request_bytes(&full.request) - 3000;
    let Err(BuildError::TooLarge { room: Some(room) }) =
        world.build(&run, (task, current), stored(&world), bound)
    else {
        panic!("the summary alone is over the bound");
    };
    let (found, allowance) = {
        let reader = Connection::open(world.scratch.db()).expect("reader");
        let read = Snapshot::begin(&reader).expect("snapshot");
        compact::rewrite(&read, &run, task, room)
            .expect("read")
            .expect("rewritten")
    };
    let answer = "say \"hi\" \\ then\n".repeat(allowance / 8);
    let fitted = compact::fit_summary(&answer, allowance).expect("fitted");
    let encoded = serde_json::to_string(&fitted).expect("json").len() - 2;
    assert_eq!(allowance, 1000, "the 4,000-byte summary is 3,000 over");
    assert!(answer.starts_with(&fitted));
    // Each 16-byte line escapes to 20, so 50 lines take the allowance; the last newline trims.
    assert_eq!(encoded, 998);
    assert_eq!(fitted.len(), 799);
    assert!(
        world
            .store
            .store_summary(task, found.generation, found.covers, &fitted, None)
            .expect("stored")
    );
    let rebuilt = world
        .build(&run, (task, current), stored(&world), bound)
        .expect("the rewritten summary fits");
    assert!(request_bytes(&rebuilt.request) <= bound);
}

/// Run `at`'s round as the store thread writes it, on connection 7.
fn round(world: &mut World, at: (ThreadId, RunId), text: &str, items: &[Value], calls: &[&str]) {
    let items: Vec<Opaque> = items
        .iter()
        .map(|item| Opaque::new(serde_json::to_vec(item).expect("json")).expect("small"))
        .collect();
    let calls: Vec<CallId> = calls
        .iter()
        .map(|call| CallId::new(call).expect("short"))
        .collect();
    let results: Vec<_> = calls
        .into_iter()
        .map(|call| {
            let unknown = crate::dispatch::ToolError::Unknown;
            (call, crate::dispatch::ToolOutput::Error(unknown))
        })
        .collect();
    world
        .store
        .append_round(at, ConnectionTag(7), Some(text), &items, &results)
        .expect("round");
}

fn said(role: &str, text: &str) -> Value {
    json!({ "type": "message", "role": role, "content": text })
}

fn call_item(call: &str) -> Value {
    json!({ "type": "function_call", "id": format!("fc_{call}"), "call_id": call, "name": "lookup", "arguments": "{}" })
}

fn result_item(call: &str) -> Value {
    json!({ "type": "function_call_output", "call_id": call, "output": "{\"error\":\"unknown tool\"}" })
}

/// The main thread's run "ask": a first round that said "Looking." and made two calls, its
/// stored native items and results, and the run left active.
fn first_round(world: &mut World) -> (ThreadId, RunId) {
    let main = world.main;
    let (_, high) = world.store.high_water().expect("ids");
    let at = (main, RunId(high.0 + 1));
    world
        .store
        .begin_turn(main, None, at.1, "ask")
        .expect("turn");
    let reasoning = json!({ "type": "reasoning", "encrypted_content": "sealed" });
    round(
        world,
        at,
        "Looking.",
        &[
            reasoning,
            said("assistant", "Looking."),
            call_item("c1"),
            call_item("c2"),
        ],
        &["c1", "c2"],
    );
    at
}

/// Round 1's native input, as the first run's replay sends it.
fn first_round_input() -> Vec<Value> {
    vec![
        said("user", "ask"),
        json!({ "type": "reasoning", "encrypted_content": "sealed" }),
        said("assistant", "Looking."),
        call_item("c1"),
        call_item("c2"),
        result_item("c1"),
        result_item("c2"),
    ]
}

/// The next run's whole request input on `connection`, as the Responses body sends it.
fn next_input(world: &World, run: RunId, connection: u64) -> Vec<Value> {
    let mut snapshot = snapshot(Origin::Kumo, None);
    snapshot.connection = ConnectionTag(connection);
    let built = world
        .build(&snapshot, (world.main, run), Supplied::default(), BOUND)
        .expect("built");
    let body = crate::engines::responses::body(&built.request).expect("body");
    body["input"].as_array().expect("a list").clone()
}

fn next_ask() -> Value {
    said(
        "user",
        &format!("next\n\nLocal time: {TIME}\nDrawings on screen: []"),
    )
}

fn partial() -> [NewEvent<'static>; 1] {
    [NewEvent {
        kind: EventKind::AssistantText,
        text: Some("Partial"),
        body: None,
    }]
}

#[test]
fn a_two_round_run_replays_natively_through_its_final_message() {
    let mut world = world("ctx-rounds-done");
    let at = first_round(&mut world);
    round(
        &mut world,
        at,
        "Found it.",
        &[said("assistant", "Found it.")],
        &[],
    );
    world
        .store
        .end_run(at.0, at.1, &[], RunEnd::Done)
        .expect("ended");
    let (_, next) = world.run(Some(world.main), "next", None);
    let mut expected = first_round_input();
    expected.extend([said("assistant", "Found it."), next_ask()]);
    assert_eq!(next_input(&world, next, 7), expected);

    world.store = Store::open(&world.scratch.db()).expect("reopened");
    assert_eq!(next_input(&world, next, 7), expected, "after a restart");
    let (page, _) = world
        .store
        .pages()
        .events_page(world.main, None, 50)
        .expect("page");
    assert_eq!(
        page.iter().map(|event| event.kind).collect::<Vec<_>>(),
        [
            PageEventKind::User,
            PageEventKind::AssistantText,
            PageEventKind::AssistantText,
            PageEventKind::User
        ],
        "the panel never sees round items"
    );
}

#[test]
fn a_run_stopped_in_its_second_round_replays_round_one_then_the_text_after_it() {
    let mut world = world("ctx-rounds-stop");
    let at = first_round(&mut world);
    let stopped = RunEnd::Interrupted(crate::store::Interruption::Stop);
    world
        .store
        .end_run(at.0, at.1, &partial(), stopped)
        .expect("ended");
    let (_, next) = world.run(Some(world.main), "next", None);
    let mut expected = first_round_input();
    expected.extend([said("assistant", "Partial"), next_ask()]);
    assert_eq!(next_input(&world, next, 7), expected);
    assert_eq!(
        next_input(&world, next, 8),
        [
            said("user", "ask"),
            said("assistant", "Looking.Partial"),
            next_ask()
        ],
        "another connection replays the run as its text only"
    );
}

#[test]
fn a_run_cut_by_a_crash_after_its_round_replays_what_was_stored() {
    let mut world = world("ctx-rounds-crash");
    let at = first_round(&mut world);
    world.store.append(at.0, at.1, &partial()).expect("text");
    world.store = Store::open(&world.scratch.db()).expect("reopened");
    world.store.recover().expect("recovered");
    let (_, next) = world.run(Some(world.main), "next", None);
    let mut expected = first_round_input();
    expected.extend([said("assistant", "Partial"), next_ask()]);
    assert_eq!(next_input(&world, next, 7), expected);
}

#[test]
fn a_bad_stored_round_record_fails_the_build() {
    let frame = r#"{"call":"c1","output":""}"#;
    let long = format!(
        r#"{{"call":"c1","output":"{}"}}"#,
        "x".repeat(65 * 1024 + 1 - frame.len())
    );
    for (kind, body) in [
        (EventKind::OutputItem, "not json"),
        (EventKind::OutputItem, r#"{"connection":7,"item":[1]}"#),
        (EventKind::OutputItem, r#"{"item":{"type":"message"}}"#),
        (EventKind::ToolResult, r#"{"call":"","output":"x"}"#),
        (EventKind::ToolResult, r#"{"call":"c1"}"#),
        (EventKind::ToolResult, long.as_str()),
    ] {
        let mut world = world("ctx-rounds-bad");
        let main = world.main;
        let (_, high) = world.store.high_water().expect("ids");
        let run = RunId(high.0 + 1);
        world
            .store
            .begin_turn(main, None, run, "ask")
            .expect("turn");
        let event = NewEvent {
            kind,
            text: None,
            body: Some(body),
        };
        world.store.append(main, run, &[event]).expect("appended");
        let built = world.build(
            &snapshot(Origin::Kumo, None),
            (main, run),
            Supplied::default(),
            BOUND,
        );
        assert!(
            matches!(built, Err(BuildError::Store(StoreError::RoundRecord))),
            "{kind:?} {built:?}"
        );
    }
}

#[test]
fn a_round_the_reader_would_refuse_is_never_written() {
    use crate::dispatch::ToolOutput;
    let mut world = world("ctx-rounds-refused");
    let at = first_round(&mut world);
    let opaque =
        |item: Value| Opaque::new(serde_json::to_vec(&item).expect("json")).expect("small");
    let call = CallId::new("c3").expect("short");
    let frame = r#"{"call":"c3","output":{"text":""}}"#;
    let fits = "x".repeat(65 * 1024 - frame.len());
    let over = "x".repeat(65 * 1024 + 1 - frame.len());
    let backwards = vec![HistoryRef {
        thread: world.main,
        first: EventId(2),
        last: EventId(1),
    }];
    for output in [ToolOutput::Text(over), ToolOutput::History(backwards)] {
        let refused = world.store.append_round(
            at,
            ConnectionTag(7),
            Some("Refused."),
            &[opaque(said("assistant", "Refused."))],
            &[(call.clone(), output)],
        );
        assert!(
            matches!(refused, Err(StoreError::RoundRecord)),
            "{refused:?}"
        );
    }
    world
        .store
        .append_round(
            at,
            ConnectionTag(7),
            None,
            &[opaque(call_item("c3"))],
            &[(call, ToolOutput::Text(fits.clone()))],
        )
        .expect("the largest result the reader takes");
    world
        .store
        .end_run(at.0, at.1, &[], RunEnd::Done)
        .expect("ended");
    let (_, next) = world.run(Some(world.main), "next", None);
    let mut expected = first_round_input();
    expected.extend([
        call_item("c3"),
        json!({ "type": "function_call_output", "call_id": "c3", "output": fits }),
        next_ask(),
    ]);
    assert_eq!(
        next_input(&world, next, 7),
        expected,
        "the refused rounds left nothing, and the thread still builds"
    );
}

#[test]
fn a_native_item_the_reader_would_refuse_is_never_written() {
    let mut world = world("ctx-rounds-native-refused");
    let at = first_round(&mut world);
    // `1e5` is legal JSON that reads back as `100000.0`, so this item grows past 256 KiB.
    let growing = format!(r#"{{"n":[{}1e5],"type":"message"}}"#, "1e5,".repeat(59_999));
    let frame = r#"{"pad":"","type":"message"}"#;
    let fits = format!(
        r#"{{"pad":"{}","type":"message"}}"#,
        "x".repeat(256 * 1024 - frame.len())
    );
    assert_eq!(fits.len(), 256 * 1024);
    let opaque = |item: &str| Opaque::new(item.as_bytes().to_vec()).expect("within the cap");
    let refused = world.store.append_round(
        at,
        ConnectionTag(7),
        Some("Refused."),
        &[opaque(&growing)],
        &[],
    );
    assert!(
        matches!(refused, Err(StoreError::RoundRecord)),
        "{refused:?}"
    );
    world
        .store
        .append_round(at, ConnectionTag(7), None, &[opaque(&fits)], &[])
        .expect("the largest item the reader takes");
    let reader = Connection::open(world.scratch.db()).expect("reader");
    let items = Snapshot::begin(&reader)
        .expect("snapshot")
        .conversation(world.main, None, usize::MAX)
        .expect("the thread still reads");
    let natives: Vec<&[u8]> = items
        .iter()
        .filter_map(|item| match &item.body {
            Body::Native { item, .. } => Some(item.bytes()),
            _ => None,
        })
        .collect();
    assert_eq!(
        natives.len(),
        5,
        "round 1's four items and the one that fits"
    );
    assert_eq!(natives[4], fits.as_bytes());
    assert!(
        !items
            .iter()
            .any(|item| item.body == Body::Assistant("Refused.".into()))
    );
}

#[test]
fn a_full_text_output_replays_as_its_text_and_an_untagged_one_as_it_was() {
    let mut world = world("ctx-rounds-text");
    let main = world.main;
    let (_, high) = world.store.high_water().expect("ids");
    let at = (main, RunId(high.0 + 1));
    world
        .store
        .begin_turn(main, None, at.1, "ask")
        .expect("turn");
    let full = "x".repeat(crate::dispatch::MAX_OUTPUT - r#"{"text":""}"#.len());
    let output = crate::dispatch::ToolOutput::Text(full.clone());
    assert_eq!(output.stored().len(), crate::dispatch::MAX_OUTPUT);
    let items: Vec<Opaque> = [call_item("c1"), call_item("c2")]
        .iter()
        .map(|item| Opaque::new(serde_json::to_vec(item).expect("json")).expect("small"))
        .collect();
    world
        .store
        .append_round(
            at,
            ConnectionTag(7),
            None,
            &items,
            &[(CallId::new("c1").expect("short"), output)],
        )
        .expect("round");
    let older = NewEvent {
        kind: EventKind::ToolResult,
        text: None,
        body: Some(r#"{"call":"c2","output":"older text"}"#),
    };
    world.store.append(main, at.1, &[older]).expect("appended");
    round(
        &mut world,
        at,
        "Found it.",
        &[said("assistant", "Found it.")],
        &[],
    );
    world
        .store
        .end_run(at.0, at.1, &[], RunEnd::Done)
        .expect("ended");
    let (_, next) = world.run(Some(world.main), "next", None);
    let output = |call: &str, text: &str| json!({ "type": "function_call_output", "call_id": call, "output": text });
    assert_eq!(
        next_input(&world, next, 7),
        [
            said("user", "ask"),
            call_item("c1"),
            call_item("c2"),
            output("c1", &full),
            output("c2", "older text"),
            said("assistant", "Found it."),
            next_ask(),
        ]
    );
}

#[test]
fn a_result_of_exactly_65_kib_is_read() {
    let mut world = world("ctx-rounds-cap");
    let main = world.main;
    let (_, high) = world.store.high_water().expect("ids");
    let run = RunId(high.0 + 1);
    world
        .store
        .begin_turn(main, None, run, "ask")
        .expect("turn");
    let frame = r#"{"call":"c1","output":""}"#;
    let body = format!(
        r#"{{"call":"c1","output":"{}"}}"#,
        "x".repeat(65 * 1024 - frame.len())
    );
    assert_eq!(body.len(), 65 * 1024);
    let event = NewEvent {
        kind: EventKind::ToolResult,
        text: None,
        body: Some(&body),
    };
    world.store.append(main, run, &[event]).expect("appended");
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, run),
            Supplied::default(),
            BOUND,
        )
        .expect("the cap fits");
    assert_eq!(
        built.request.input,
        [message(
            Role::User,
            &format!("ask\n\nLocal time: {TIME}\nDrawings on screen: []")
        )],
        "a result with no native call isn't replayed"
    );
}

/// Each deletion note as a request renders it, oldest first. The time is read back from the row,
/// so the test pins placement and wording, not the clock.
fn rendered_notes(world: &World) -> Vec<InputItem> {
    let conn = world.store.conn_for_tests();
    let mut statement = conn
        .prepare(
            "SELECT strftime('%Y-%m-%d %H:%M', created_ms / 1000, 'unixepoch', 'localtime')
             FROM events WHERE kind = 'task_deleted' ORDER BY id",
        )
        .expect("prepared");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("rows")
        .map(|when| {
            let when = when.expect("time");
            assert_eq!(when.len(), "2026-11-06 01:23".len(), "minutes, local");
            message(
                Role::Developer,
                &format!("Note: a task was deleted at {when}."),
            )
        })
        .collect()
}

impl World {
    /// Starts a Kumo run with its ask and leaves it active.
    fn start(&mut self, ask: &str) -> RunId {
        let (_, high) = self.store.high_water().expect("ids");
        let run = RunId(high.0 + 1);
        self.store
            .begin_turn(self.main, None, run, ask)
            .expect("turn");
        run
    }

    fn reply(&mut self, run: RunId, reply: &str) {
        let events = [NewEvent {
            kind: EventKind::AssistantText,
            text: Some(reply),
            body: None,
        }];
        self.store
            .end_run(self.main, run, &events, RunEnd::Done)
            .expect("ended");
    }
}

#[test]
fn a_deletion_note_follows_the_run_it_interrupted_and_precedes_the_current_one() {
    let mut world = world("ctx-notes");
    let main = world.main;
    let (b, _) = world.run(None, "task b", None);
    let (c, _) = world.run(None, "task c", None);
    world.run(Some(main), "ask one", Some("reply one"));
    let two = world.start("ask two");
    world.store.begin_delete(b).expect("b hidden");
    world.reply(two, "reply two");
    let three = world.start("ask three");
    world.store.begin_delete(c).expect("c hidden");
    let notes = rendered_notes(&world);
    assert_eq!(notes.len(), 2);
    let run = snapshot(Origin::Kumo, None);
    let built = world
        .build(&run, (main, three), Supplied::default(), BOUND)
        .expect("built");
    assert_eq!(
        built.request.input,
        [
            message(Role::User, "ask one"),
            message(Role::Assistant, "reply one"),
            message(Role::User, "ask two"),
            message(Role::Assistant, "reply two"),
            notes[0].clone(),
            notes[1].clone(),
            message(
                Role::User,
                &format!("ask three\n\nLocal time: {TIME}\nDrawings on screen: []")
            ),
        ],
        "no run is split, and a note made mid-turn waits before it"
    );
    assert_eq!(built.sources, [main]);

    let reopened = Store::open(&world.scratch.db()).expect("reopened");
    drop(std::mem::replace(&mut world.store, reopened));
    let recovered = world.store.recover().expect("recovered");
    assert_eq!(recovered.finished_deletions, 2);
    assert_eq!(rendered_notes(&world), notes, "one note per delete, kept");
    let four = world.start("ask four");
    let built = world
        .build(&run, (main, four), Supplied::default(), BOUND)
        .expect("built");
    assert_eq!(
        built.request.input,
        [
            message(Role::User, "ask one"),
            message(Role::Assistant, "reply one"),
            message(Role::User, "ask two"),
            message(Role::Assistant, "reply two"),
            notes[0].clone(),
            message(Role::User, "ask three"),
            notes[1].clone(),
            message(
                Role::User,
                &format!("ask four\n\nLocal time: {TIME}\nDrawings on screen: []")
            ),
        ],
        "once its run ended, the second note follows it"
    );
}

#[test]
fn a_summary_covers_the_notes_through_its_cutoff_and_a_later_note_still_renders() {
    let mut world = world("ctx-note-summary");
    let main = world.main;
    let (b, _) = world.run(None, "task b", None);
    world.run(Some(main), "ask one", Some("reply one"));
    world.store.begin_delete(b).expect("hidden");
    let (_, current) = world.run(Some(main), "ask two", None);
    let events = world.events(main);
    let note = rendered_notes(&world).remove(0);
    let run = snapshot(Origin::Kumo, None);
    let through = |last: EventId| Supplied {
        summary: Some(Summary {
            first: events[0],
            last,
            text: "earlier".to_owned(),
        }),
        items: Vec::new(),
    };
    let summarized = |last: EventId| {
        message(
            Role::Developer,
            &format!("Summary of events {}–{}:\nearlier", events[0].0, last.0),
        )
    };
    let ask = message(
        Role::User,
        &format!("ask two\n\nLocal time: {TIME}\nDrawings on screen: []"),
    );
    let below = world
        .build(&run, (main, current), through(events[1]), BOUND)
        .expect("built");
    assert_eq!(
        below.request.input,
        [summarized(events[1]), note, ask.clone()]
    );
    let covered = world
        .build(&run, (main, current), through(events[2]), BOUND)
        .expect("built");
    assert_eq!(covered.request.input, [summarized(events[2]), ask]);
}

#[test]
fn the_newest_32_notes_are_kept_and_charged_to_the_bound() {
    let mut world = world("ctx-note-bound");
    let main = world.main;
    let tasks: Vec<_> = (0..33).map(|_| world.run(None, "task", None).0).collect();
    world.run(Some(main), "ask one", Some("reply one"));
    world.store.begin_delete(tasks[0]).expect("oldest hidden");
    world.run(Some(main), "ask two", None);
    for &task in &tasks[1..] {
        world.store.begin_delete(task).expect("hidden");
    }
    let (_, current) = world.run(Some(main), "ask three", None);
    let notes = rendered_notes(&world);
    assert_eq!(notes.len(), 33);
    let ask = message(
        Role::User,
        &format!("ask three\n\nLocal time: {TIME}\nDrawings on screen: []"),
    );
    let newest: Vec<_> = std::iter::once(message(Role::User, "ask two"))
        .chain(notes[1..].iter().cloned())
        .chain([ask])
        .collect();
    let mut all = vec![
        message(Role::User, "ask one"),
        message(Role::Assistant, "reply one"),
    ];
    all.extend(newest.iter().cloned());
    let run = snapshot(Origin::Kumo, None);
    let wide = world
        .build(&run, (main, current), Supplied::default(), BOUND)
        .expect("built");
    assert_eq!(
        wide.request.input, all,
        "the oldest note is past the 32 kept"
    );
    let size = request_bytes(&wide.request);
    let exact = world
        .build(&run, (main, current), Supplied::default(), size)
        .expect("fits exactly");
    assert_eq!(exact.request.input, all);
    let under = world
        .build(&run, (main, current), Supplied::default(), size - 1)
        .expect("built");
    assert_eq!(
        under.request.input, newest,
        "one byte short drops the oldest run, and the notes count"
    );
}

#[test]
fn compaction_summarizes_a_note_inside_its_range_and_leaves_a_later_one() {
    let mut world = world("compact-notes");
    let main = world.main;
    let (b, _) = world.run(None, "task b", None);
    let (c, _) = world.run(None, "task c", None);
    let one = world.start("ask one");
    world.store.begin_delete(b).expect("b hidden");
    world.reply(one, "reply one");
    world.store.begin_delete(c).expect("c hidden");
    world.runs(main, 8, 4);
    let notes = rendered_notes(&world);
    let found = world
        .compact(&snapshot(Origin::Kumo, None), main, 1)
        .expect("due");
    let events = world.events(main);
    assert_eq!(found.covers, (events[0], events[2]), "the oldest run only");
    assert_eq!(
        found.built.request.input,
        [
            message(Role::User, "ask one"),
            message(Role::Assistant, "reply one"),
            notes[0].clone(),
            message(Role::Developer, WRITE_NOW),
        ],
        "the note inside the run follows it, and the later one waits"
    );
}
