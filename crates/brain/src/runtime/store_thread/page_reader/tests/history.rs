use super::*;
use crate::coordinator::{BuildKey, Coordinator, Effect};
use crate::dispatch::{Caller, Dispatch, ToolError, ToolOutput};
use crate::engines::{CallId, Finish, Opaque, ToolName, TurnEvent};
use crate::recall::Selection;

mod wire;

const R: RunId = RunId(10);
const MAIN: ThreadId = ThreadId(1);
const SOURCE: ThreadId = ThreadId(2);

#[test]
fn a_real_read_call_ends_its_stamp_wait_on_cancellation_and_deadline() {
    for expired in [false, true] {
        let (_scratch, running, mut core, key) =
            active("history-stamp-ending", "small", Prior::Empty);
        let mut reads = calls(
            &mut core,
            key,
            1,
            ToolName::ReadHistory,
            r#"{"task":2,"from":1,"to":1}"#,
        );
        let call = reads.pop().unwrap();
        let control = call.control.clone();
        let release = gate(&running.gates, is_turn);
        assert!(running.thread.send(turn(20)));
        running.saw(|seen| matches!(seen, Seen::StoreOpStarted(op) if is_turn(op)));
        assert!(running.thread.send(StoreOp::ReadCall(call)));
        running.saw(|seen| *seen == Seen::ReadWaiting);
        let expected = if expired {
            *control.test.time.lock().unwrap() = Some(control.end);
            crate::recall::Error::Timeout
        } else {
            control.cancel();
            crate::recall::Error::Ended
        };
        let reply = running.ack();
        let Input::ReadFinished(done) = reply else {
            panic!("a read reply: {reply:?}");
        };
        assert_eq!(done.result, Err(expected));
        release.send(()).unwrap();
        assert_eq!(running.ack(), Input::TurnStored { run: RunId(20) });
        running.thread.close().blocking_recv().unwrap();
    }
}

#[test]
fn actual_model_builds_refuse_create_and_focus_without_a_commit_or_tool_row() {
    let (scratch, running, mut core, _key) =
        active("history-offer-refusals", "small", Prior::Empty);
    for (name, id, args) in [
        (
            ToolName::CreateTask,
            "create",
            r#"{"title":"new task","focus":false}"#,
        ),
        (ToolName::Focus, "focus", r#"{"task":2}"#),
    ] {
        for _ in 0..2 {
            let mut effects = Vec::new();
            assert_eq!(
                core.tool_call(
                    Caller::Model {
                        run: R,
                        call: CallId::new(id).unwrap()
                    },
                    name,
                    args.as_bytes(),
                    &mut effects
                ),
                Dispatch::Answered(ToolOutput::Error(ToolError::NotHere))
            );
            assert_eq!(effects, Vec::new());
        }
    }
    running.thread.close().blocking_recv().unwrap();
    assert_eq!(recorded(&scratch), (0, 0));
}

#[derive(Clone, Copy)]
enum Prior {
    Empty,
    Reference,
}

fn recorded(scratch: &Scratch) -> (i64, i64) {
    let reader = rusqlite::Connection::open(scratch.db()).unwrap();
    (
        reader
            .query_row("SELECT count(*) FROM tool_calls WHERE run=10", [], |row| {
                row.get(0)
            })
            .unwrap(),
        reader
            .query_row(
                "SELECT count(*) FROM events WHERE run=10 AND kind='tool_result'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
    )
}

fn active(name: &str, text: &str, prior: Prior) -> (Scratch, Running, Coordinator, BuildKey) {
    active_on(name, text, prior, MAIN)
}

fn active_on(
    name: &str,
    text: &str,
    prior: Prior,
    thread: ThreadId,
) -> (Scratch, Running, Coordinator, BuildKey) {
    let scratch = Scratch::new(name);
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    assert_eq!(store.main_thread().unwrap(), MAIN);
    store
        .begin_turn(SOURCE, Some(&"t".repeat(80)), RunId(1), text)
        .unwrap();
    store.end_run(SOURCE, RunId(1), &[], RunEnd::Done).unwrap();
    if matches!(prior, Prior::Reference) {
        store.begin_turn(MAIN, None, RunId(2), "prior").unwrap();
        store.append_round((MAIN, RunId(2)), crate::context::Route { kind: kumochi_protocol::message::ConnectionKind::ChatGpt, tag: ConnectionTag(0) }, None,
            &[Opaque::new(br#"{"type":"function_call","call_id":"prior","name":"read_history","arguments":"{}"}"#.to_vec()).unwrap()],
            &[(CallId::new("prior").unwrap(), ToolOutput::Recall(Selection::Read { task: 2, first_shown: Some(1), last_shown: Some(1), cut: None, next: None }))]).unwrap();
        store.end_run(MAIN, RunId(2), &[], RunEnd::Done).unwrap();
    }
    store.begin_turn(thread, None, R, "read").unwrap();
    let running = run(store, None);
    let mut core = Coordinator::ready_for_tests(MAIN, SOURCE, RunId(9), Vec::new());
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(thread),
        text: "read".into(),
    })
    .unwrap();
    let effects = core.handle(Input::TurnStored { run: R }).unwrap();
    let [Effect::Store(StoreOp::BuildContext(build))] = effects.as_slice() else {
        panic!("build: {effects:?}");
    };
    let key = build.key;
    let mut build = build.clone();
    build.snapshot = Some(RunSnapshot {
        origin: if thread == MAIN {
            Origin::Kumo
        } else {
            Origin::Task(thread)
        },
        assistant: AssistantName::new("Kumo").unwrap(),
        user: None,
        choice: None,
        connection: ConnectionTag(0),
    });
    assert!(running.thread.send(StoreOp::BuildContext(build)));
    let reply = running.ack();
    let Input::ContextBuilt(ContextBuilt {
        result: Ok(request),
        ..
    }) = &reply
    else {
        panic!("built: {reply:?}");
    };
    assert_eq!(
        request.sources,
        if matches!(prior, Prior::Reference) {
            vec![MAIN, SOURCE]
        } else {
            vec![thread]
        }
    );
    core.handle(reply).unwrap();
    (scratch, running, core, key)
}

fn calls(
    core: &mut Coordinator,
    key: BuildKey,
    count: u32,
    name: ToolName,
    args: &str,
) -> Vec<crate::dispatch::ReadCall> {
    for n in 0..count {
        let call = CallId::new(&format!("read-{n}")).unwrap();
        let wire = match name {
            ToolName::ReadHistory => "read_history",
            ToolName::SearchHistory => "search_history",
            _ => unreachable!(),
        };
        let item = Opaque::new(serde_json::json!({ "type": "function_call", "call_id": call.as_str(), "name": wire, "arguments": args }).to_string().into_bytes()).unwrap();
        assert_eq!(
            core.handle(Input::Engine {
                key,
                event: TurnEvent::OutputItem { index: n, item }
            }),
            Ok(Vec::new())
        );
        for event in [
            TurnEvent::ToolCallStarted {
                call: call.clone(),
                name,
            },
            TurnEvent::ToolCallReady {
                call,
                args: args.into(),
            },
        ] {
            assert_eq!(core.handle(Input::Engine { key, event }), Ok(Vec::new()));
        }
    }
    core.handle(Input::Engine {
        key,
        event: TurnEvent::Finished(Finish::Completed),
    })
    .unwrap()
    .into_iter()
    .map(|effect| match effect {
        Effect::Store(StoreOp::ReadCall(call)) => call,
        other => panic!("read only: {other:?}"),
    })
    .collect()
}

#[test]
fn sixteen_real_reads_end_in_budget_without_dropping_results_or_failing_the_store() {
    for text in [
        "x".repeat(40 * 1024),
        "界".repeat(14 * 1024),
        "\"\\\u{1}".repeat(14 * 1024),
    ] {
        let (scratch, running, mut core, key) =
            active("history-sixteen-budget", &text, Prior::Empty);
        let reads = calls(
            &mut core,
            key,
            16,
            ToolName::ReadHistory,
            r#"{"task":2,"from":1,"to":1}"#,
        );
        assert_eq!(reads.len(), 16);
        for call in reads {
            assert!(running.thread.send(StoreOp::ReadCall(call)));
        }
        let mut committed = 0;
        let mut outputs = 0;
        loop {
            let reply = running.ack();
            if matches!(
                reply,
                Input::CallCommitted {
                    outcome: crate::dispatch::Committed::Done,
                    ..
                }
            ) {
                committed += 1;
            }
            if let Input::ContextBuilt(ContextBuilt { result, .. }) = &reply {
                assert_eq!(result, &Err(Unbuilt::TooLarge { room: None }));
            }
            let effects = core.handle(reply).unwrap();
            let mut terminal = false;
            for effect in effects {
                match effect {
                    Effect::Store(StoreOp::RoundItems(ref round)) => {
                        assert_eq!(round.calls.len(), 16);
                        outputs = round.calls.len();
                        assert!(
                            round
                                .calls
                                .iter()
                                .all(|(_, output)| matches!(output, ToolOutput::Recall(_)))
                        );
                    }
                    Effect::Store(
                        ref op @ StoreOp::EndRun {
                            end: RunEnd::Failed(crate::store::Failure::Budget),
                            ..
                        },
                    ) => {
                        assert!(running.thread.send(op.clone()));
                        terminal = true;
                    }
                    Effect::Store(ref op) => {
                        assert!(running.thread.send(op.clone()));
                    }
                    _ => {}
                }
                if let Effect::Store(StoreOp::RoundItems(round)) = effect {
                    assert!(running.thread.send(StoreOp::RoundItems(round)));
                }
            }
            if terminal {
                break;
            }
        }
        assert_eq!((committed, outputs), (16, 16));
        assert_eq!(core.current_run(), None);
        running.thread.close().blocking_recv().unwrap();
        assert_eq!(recorded(&scratch), (16, 16));
    }
}

#[test]
fn two_barriers_fence_late_success_after_a_calling_source_delete_without_a_tool_row() {
    let (scratch, running, mut core, key) = active(
        "history-late-source-delete",
        "needle B private",
        Prior::Reference,
    );
    let args = r#"{"query":"nonmatching"}"#;
    let mut reads = calls(&mut core, key, 1, ToolName::SearchHistory, args);
    let call = reads.pop().unwrap();
    let release_read = gate(&running.read_gates, |op| matches!(op, StoreOp::ReadCall(_)));
    let release_delete = gate(&running.gates, |op| matches!(op, StoreOp::Delete { .. }));
    assert!(running.thread.send(StoreOp::ReadCall(call.clone())));
    running.saw(|seen| *seen == Seen::ReadHeld);
    release_read.send(()).unwrap();
    let done = running.ack();
    let Input::ReadFinished(ref finished) = done else {
        panic!("completed: {done:?}");
    };
    assert_eq!(
        finished.result,
        Ok((
            Selection::Search {
                query: "nonmatching".into(),
                hits: Vec::new(),
                more: false
            },
            Vec::new()
        ))
    );
    let effects = core
        .handle(Input::Delete {
            thread: SOURCE,
            after_turns: 1,
            resend: false,
        })
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::Store(StoreOp::Delete { thread: SOURCE })]
    );
    assert!(running.thread.send(StoreOp::Delete { thread: SOURCE }));
    running.saw(|seen| {
        matches!(
            seen,
            Seen::StoreOpStarted(StoreOp::Delete { thread: SOURCE })
        )
    });
    assert_eq!(
        core.handle(done.clone()),
        Ok(vec![Effect::CancelEngine { run: R }])
    );
    assert_eq!(core.handle(done.clone()), Ok(Vec::new()));
    release_delete.send(()).unwrap();
    let deleted = running.ack();
    assert!(matches!(deleted, Input::Deleted { thread: SOURCE, .. }));
    let effects = core.handle(deleted).unwrap();
    let builds = effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build),
            Effect::Control(_) => None,
            other => panic!("rebuild only: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(builds.len(), 1);
    assert_ne!(builds[0].key, call.build);
    assert!(
        running
            .thread
            .send(StoreOp::BuildContext(builds[0].clone()))
    );
    core.handle(running.ack()).unwrap();
    let mut effects = Vec::new();
    assert_eq!(
        core.tool_call(
            Caller::Model {
                run: R,
                call: CallId::new("read-0").unwrap()
            },
            ToolName::SearchHistory,
            args.as_bytes(),
            &mut effects
        ),
        Dispatch::Answered(ToolOutput::Error(ToolError::Ended))
    );
    assert_eq!(effects, Vec::new());
    assert_eq!(core.handle(done), Ok(Vec::new()));
    running.thread.close().blocking_recv().unwrap();
    assert_eq!(recorded(&scratch), (0, 0));
}
