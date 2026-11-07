use super::*;
use crate::connections::model::sse::Reader;
use crate::engines::responses::Round;
use crate::recall::Hit;
use serde_json::json;
use std::fmt::Write as _;

fn wire(core: &mut Coordinator, name: &str, args: &str) -> Vec<Effect> {
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
            effects.extend(core.handle(Input::Engine { run: R, event }).unwrap());
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
    let (scratch, running, mut core) = active_on(
        "history-wire-success",
        "wire-private-source-marker small",
        Prior::Empty,
        thread,
    );
    let effects = wire(&mut core, name, args);
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
        let (scratch, running, mut core) = active_on(
            "history-wire-refusal",
            "wire-private-source-marker small",
            Prior::Empty,
            thread,
        );
        let effects = wire(&mut core, name, args);
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
