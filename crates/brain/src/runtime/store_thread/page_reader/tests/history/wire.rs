use super::super::super::context::{read as build_request, read_job};
use super::*;
use crate::config::tuning::ContextTuning;
use crate::connections::model::sse::Reader;
use crate::coordinator::{Arrived, JobKey, JobKind, ReadJob};
use crate::engines::{claude_code::seed, responses};
use crate::recall::Hit;
use crate::recall::control::Control;
use kumochi_protocol::message::{ConnectionKind, Deleted, EventId};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::Instant;

use responses::Round;

fn wire(core: &mut Coordinator, key: BuildKey, name: &str, args: &str) -> Vec<Effect> {
    let item = json!({"type":"function_call", "id":"fc-wire", "call_id":"wire", "name":name, "arguments":args});
    let mut added = item.clone();
    added["arguments"] = json!("");
    let mut stream = String::new();
    for event in [
        json!({"type":"response.output_item.added", "item":added}),
        json!({"type":"response.function_call_arguments.delta", "item_id":"fc-wire", "delta":args}),
        json!({"type":"response.function_call_arguments.done", "item_id":"fc-wire", "arguments":args}),
        json!({"type":"response.output_item.done", "output_index":0, "item":item}),
        json!({"type":"response.completed"}),
    ] {
        write!(stream, "data: {event}\n\n").unwrap();
    }
    let mut reader = Reader::default();
    let mut round = Round::default();
    let mut effects = Vec::new();
    let mut ended = false;
    for bytes in stream.as_bytes().chunks(7) {
        let read = reader.feed(bytes, &mut |event| round.event(event));
        let mut events = round.take();
        if let Some(end) = round.end(read) {
            events.push(end);
            ended = true;
        }
        for event in events {
            effects.extend(core.handle(Input::Engine { key, event }).unwrap());
        }
    }
    assert!(ended);
    effects
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixture keeps wire, ledger and commit ordering assertions together"
)]
fn succeeds(thread: ThreadId, name: &str, args: &str, selection: Selection) {
    let (scratch, running, mut core, key) = active_on(
        "history-wire-success",
        "wire-private-source-marker small",
        Prior::Empty,
        thread,
    );
    let effects = wire(&mut core, key, name, args);
    let [Effect::Store(StoreOp::ReadCall(call))] = effects.as_slice() else {
        panic!("wire must reach the bounded reader: {effects:?}");
    };
    assert_eq!(call.thread, thread);
    assert_eq!(call.build.run, R);
    assert_eq!(call.tool, name);
    assert_eq!(
        call.origin,
        if thread == MAIN {
            Origin::Kumo
        } else {
            Origin::Task(SOURCE)
        }
    );
    assert_eq!(call.control.check(), Ok(()));
    let release = gate(&running.read_gates, |op| matches!(op, StoreOp::ReadCall(_)));
    assert!(running.thread.send(StoreOp::ReadCall(call.clone())));
    running.saw(|seen| *seen == Seen::ReadHeld);
    let caller = Caller::Model {
        run: R,
        call: CallId::new("wire").unwrap(),
    };
    let mut retry_effects = Vec::new();
    assert_eq!(
        core.tool_call(
            caller.clone(),
            crate::dispatch::named(name).unwrap(),
            format!(" {args} ").as_bytes(),
            &mut retry_effects
        ),
        Dispatch::Joined
    );
    assert_eq!(retry_effects, Vec::new());
    assert_eq!(recorded(&scratch), (0, 0));
    release.send(()).unwrap();
    let done = running.ack();
    let Input::ReadFinished(ref read) = done else {
        panic!("read completion: {done:?}")
    };
    assert_eq!(read.build, call.build);
    assert_eq!(read.result, Ok((selection.clone(), vec![SOURCE])));
    let effects = core.handle(done).unwrap();
    let [Effect::Store(StoreOp::CommitCall(commit))] = effects.as_slice() else {
        panic!("commit only, not an answer: {effects:?}")
    };
    assert_eq!(commit.write, crate::dispatch::LocalWrite::None);
    assert_eq!(commit.then, None);
    assert_eq!(commit.output, ToolOutput::Recall(selection.clone()));
    assert_eq!(recorded(&scratch), (0, 0));
    assert_eq!(
        core.tool_call(
            caller,
            crate::dispatch::named(name).unwrap(),
            args.as_bytes(),
            &mut retry_effects
        ),
        Dispatch::Joined
    );
    assert_eq!(retry_effects, Vec::new());
    assert!(running.thread.send(StoreOp::CommitCall(commit.clone())));
    let ack = running.ack();
    assert!(matches!(
        ack,
        Input::CallCommitted {
            outcome: crate::dispatch::Committed::Done,
            ..
        }
    ));
    let effects = core.handle(ack).unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("answer after commit acknowledgement: {effects:?}")
    };
    assert_eq!(
        round.calls,
        vec![(
            CallId::new("wire").unwrap(),
            ToolOutput::Recall(selection.clone())
        )]
    );
    assert!(running.thread.send(StoreOp::RoundItems(round.clone())));
    assert!(matches!(running.ack(), Input::RoundStored { .. }));
    running.thread.close().blocking_recv().unwrap();
    assert_eq!(recorded(&scratch), (1, 1));
    let db = rusqlite::Connection::open(scratch.db()).unwrap();
    let stored: String = db
        .query_row("SELECT result FROM tool_calls WHERE run=10", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        ToolOutput::from_stored(&stored),
        Some(ToolOutput::Recall(selection))
    );
    assert!(!stored.contains("wire-private-source-marker"));
    let count: i64 = db
        .query_row("SELECT count(*) FROM threads", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

fn search(thread: ThreadId) {
    succeeds(
        thread,
        "search_history",
        r#"{"query":"small"}"#,
        Selection::Search {
            query: "small".into(),
            hits: vec![Hit { task: 2, event: 1 }],
            more: false,
        },
    );
}

fn read(thread: ThreadId) {
    succeeds(
        thread,
        "read_history",
        r#"{"task":2,"from":1,"to":1}"#,
        Selection::Read {
            task: 2,
            first_shown: Some(1),
            last_shown: Some(1),
            cut: None,
            next: None,
        },
    );
}

#[test]
fn wire_kumo_search_reaches_the_reader_ledger_and_reference_commit() {
    search(MAIN);
}

#[test]
fn wire_task_search_reaches_the_reader_ledger_and_reference_commit() {
    search(SOURCE);
}

#[test]
fn wire_kumo_read_reaches_the_reader_ledger_and_reference_commit() {
    read(MAIN);
}

#[test]
fn wire_task_read_reaches_the_reader_ledger_and_reference_commit() {
    read(SOURCE);
}

#[test]
fn wire_kumo_list_reaches_the_reader_ledger_and_reference_commit() {
    succeeds(
        MAIN,
        "list_tasks",
        "{}",
        Selection::Tasks {
            tasks: vec![2],
            truncated: false,
        },
    );
}

fn next_request(store: &Store, run: RunId, kind: ConnectionKind) -> Built {
    let connection = match kind {
        ConnectionKind::ChatGpt => ConnectionTag(0),
        ConnectionKind::ClaudeCode => ConnectionTag(1),
        ConnectionKind::AnthropicKey => unreachable!(),
    };
    build_request(
        Some(&store.side()),
        BuildContext {
            control: Control::new(Instant::now() + Duration::from_secs(10)),
            key: BuildKey {
                run,
                round: 1,
                attempt: 1,
            },
            thread: MAIN,
            origin: Origin::Kumo,
            snapshot: Some(RunSnapshot {
                origin: Origin::Kumo,
                assistant: AssistantName::new("Kumo").unwrap(),
                user: None,
                choice: Some(ModelChoice {
                    connection: kind,
                    model: ModelSlug::new("fixture").unwrap(),
                    effort: Effort::Low,
                }),
                connection,
            }),
            first: true,
            arrived: Arrived::of(kind, connection),
            drawings: crate::drawing::Digest::default(),
            picture_models: Vec::new(),
            key_results: Vec::new(),
        },
        ContextTuning::default(),
        || {},
    )
    .result
    .unwrap()
}

fn wire_output(body: &Value) -> Value {
    let mut outputs = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "wire");
    let output = outputs.next().expect("stored wire recall");
    assert_eq!(outputs.count(), 0, "only one stored wire recall");
    serde_json::from_str(output["output"].as_str().unwrap()).unwrap()
}

fn store_wire_recall(running: &Running, core: &mut Coordinator, key: BuildKey) {
    let effects = wire(core, key, "read_history", r#"{"task":2,"from":1,"to":1}"#);
    let [Effect::Store(StoreOp::ReadCall(call))] = effects.as_slice() else {
        panic!("the wire call did not reach the reader");
    };
    assert!(running.thread.send(StoreOp::ReadCall(call.clone())));
    let effects = core.handle(running.ack()).unwrap();
    let [Effect::Store(StoreOp::CommitCall(commit))] = effects.as_slice() else {
        panic!("the read was not committed");
    };
    assert!(running.thread.send(StoreOp::CommitCall(commit.clone())));
    let effects = core.handle(running.ack()).unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("the committed result did not reach the round");
    };
    assert!(running.thread.send(StoreOp::RoundItems(round.clone())));
    assert_eq!(running.ack(), Input::RoundStored { run: R, round: 1 });
}

fn compact_quoted_reply(store: &mut Store, quote: &str) {
    let compact = read_job(
        Some(&store.side()),
        ReadJob {
            control: Control::new(Instant::now() + Duration::from_secs(10)),
            key: JobKey {
                thread: MAIN,
                serial: 1,
            },
            kind: JobKind::Compact,
            origin: Origin::Kumo,
            snapshot: Some(RunSnapshot {
                origin: Origin::Kumo,
                assistant: AssistantName::new("Kumo").unwrap(),
                user: None,
                choice: None,
                connection: ConnectionTag(0),
            }),
            compact_at: 0,
        },
        ContextTuning::default(),
        || {},
    )
    .result
    .unwrap()
    .unwrap();
    assert_eq!(compact.covers, Some((EventId(2), EventId(3))));
    assert_eq!(compact.built.sources, [MAIN]);
    assert_eq!(
        compact.built.request.input,
        [
            InputItem::Message {
                role: Role::User,
                text: "prior".into()
            },
            InputItem::Message {
                role: Role::Assistant,
                text: quote.into()
            },
            InputItem::Message {
                role: Role::Developer,
                text: "Write the new summary now.".into()
            },
        ]
    );
    // A committed Kumo reply or summary may retain its quoted text after the source is deleted.
    assert!(
        store
            .store_summary(
                MAIN,
                compact.generation,
                compact.covers.unwrap(),
                quote,
                None
            )
            .unwrap()
    );
}

fn delete_source(store: Store, last_run: RunId) {
    let running = run(store, None);
    let mut core = Coordinator::ready_for_tests(MAIN, SOURCE, last_run, Vec::new());
    let effects = core
        .handle(Input::Delete {
            thread: SOURCE,
            after_turns: 0,
            resend: false,
        })
        .unwrap();
    let [Effect::Store(op @ StoreOp::Delete { thread: SOURCE })] = effects.as_slice() else {
        panic!("the source delete was not admitted");
    };
    assert!(running.thread.send(op.clone()));
    let deleted = running.ack();
    assert_eq!(
        deleted,
        Input::Deleted {
            thread: SOURCE,
            outcome: Deleted::Removed,
            last_run: Some(RunId(1))
        }
    );
    core.handle(deleted).unwrap();
    running.thread.close().blocking_recv().unwrap();
}

#[test]
fn deleted_wire_recall_is_not_replayed_after_compaction_and_restart() {
    const RAW: &str = "raw-only-marker-19";
    const QUOTE: &str = "committed-quote-marker-23";
    const PROMPT: &str = "next-user-marker-31";
    let (scratch, running, mut core, key) = active(
        "history-wire-compact-delete",
        &format!("{RAW} {QUOTE}"),
        Prior::CompactableQuote(QUOTE),
    );
    store_wire_recall(&running, &mut core, key);
    running.thread.close().blocking_recv().unwrap();

    let mut store = Store::open(&scratch.db()).unwrap();
    store.end_run(MAIN, R, &[], RunEnd::Done).unwrap();
    compact_quoted_reply(&mut store, QUOTE);

    store
        .begin_turn(MAIN, None, RunId(18), "before deletion")
        .unwrap();
    let live = next_request(&store, RunId(18), ConnectionKind::ChatGpt);
    assert_eq!(live.sources, [MAIN, SOURCE]);
    let live_body = responses::body(&live.request).unwrap();
    assert_eq!(
        wire_output(&live_body)["quoted_history"][0]["events"][0]["text"],
        format!("{RAW} {QUOTE}")
    );
    store.end_run(MAIN, RunId(18), &[], RunEnd::Done).unwrap();

    delete_source(store, RunId(18));
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    store.begin_turn(MAIN, None, RunId(19), PROMPT).unwrap();
    let responses = next_request(&store, RunId(19), ConnectionKind::ChatGpt);
    let claude = next_request(&store, RunId(19), ConnectionKind::ClaudeCode);
    let body = responses::body(&responses.request).unwrap();
    let payload = body.to_string();
    let bytes = seed::encode(&claude.request, &claude.request.tools);
    let stream = std::str::from_utf8(&bytes).unwrap();
    assert!(
        !payload.contains(RAW),
        "Responses replayed deleted source text"
    );
    assert!(
        !stream.contains(RAW),
        "Claude Code replayed deleted source text"
    );
    assert_eq!(responses.sources, [MAIN]);
    assert_eq!(claude.sources, [MAIN]);
    assert_eq!(payload.matches(QUOTE).count(), 1);
    assert_eq!(stream.matches(QUOTE).count(), 1);
    assert_eq!(
        wire_output(&body),
        json!({
            "quoted_history": [{"task": 2, "note": "[removed with a deleted task]"}],
            "note": null, "duplicate_titles": false, "truncated": false
        })
    );
    assert!(!stream.contains("tool result, quoted data"));
    let inputs = body["input"].as_array().unwrap();
    assert!(inputs.contains(&json!({
        "type": "message", "role": "developer", "content": format!("Summary of events 2–3:\n{QUOTE}")
    })));
    let user = inputs.last().unwrap();
    assert_eq!(user["role"], "user");
    assert_eq!(
        user["content"].as_str().unwrap().lines().next(),
        Some(PROMPT)
    );
    let seed: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(seed["message"]["role"], "user");
    let blocks = seed["message"]["content"].as_array().unwrap();
    assert!(blocks.contains(&json!({
        "type": "text", "text": format!("developer:\nSummary of events 2–3:\n{QUOTE}")
    })));
    assert_eq!(
        blocks.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .lines()
            .take(2)
            .collect::<Vec<_>>(),
        ["user:", PROMPT]
    );
}

#[test]
fn wire_unknown_and_unoffered_calls_cannot_read_commit_create_or_focus() {
    for (thread, name, args, expected) in [
        (MAIN, "unknown-wire-tool", "{}", ToolError::Unknown),
        (
            MAIN,
            "create_task",
            r#"{"title":"must not exist","focus":true}"#,
            ToolError::NotHere,
        ),
        (MAIN, "focus", r#"{"task":2}"#, ToolError::NotHere),
        (SOURCE, "list_tasks", "{}", ToolError::NotHere),
    ] {
        let (scratch, running, mut core, key) = active_on(
            "history-wire-refusal",
            "wire-private-source-marker small",
            Prior::Empty,
            thread,
        );
        let effects = wire(&mut core, key, name, args);
        let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
            panic!("refusal only, no read or effect: {effects:?}")
        };
        assert_eq!(
            round.calls,
            vec![(CallId::new("wire").unwrap(), ToolOutput::Error(expected))]
        );
        assert_eq!(recorded(&scratch), (0, 0));
        let mut retries = Vec::new();
        assert_eq!(
            core.tool_call(
                Caller::Model {
                    run: R,
                    call: CallId::new("wire").unwrap()
                },
                ToolName::ReadHistory,
                br#"{"task":2,"from":1,"to":1}"#,
                &mut retries
            ),
            Dispatch::Answered(ToolOutput::Error(expected))
        );
        assert_eq!(retries, Vec::new());
        assert!(running.thread.send(StoreOp::RoundItems(round.clone())));
        assert!(matches!(running.ack(), Input::RoundStored { .. }));
        running.thread.close().blocking_recv().unwrap();
        assert_eq!(recorded(&scratch), (0, 1));
        let db = rusqlite::Connection::open(scratch.db()).unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
