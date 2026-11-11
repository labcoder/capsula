use super::*;
use crate::dispatch::{self, Caller, Ledger, Route, Step, ToolOutput};
use crate::engines::CallId;
use kumochi_protocol::message::RunId;

#[test]
fn closed_history_arguments_reject_foreign_fields_duplicates_and_bad_shapes() {
    type Prepare = fn(&[u8]) -> Result<Prepared, ToolError>;
    let search: Prepare = search;
    let read: Prepare = read;
    let tasks: Prepare = tasks;
    for (prepare, bad) in [
        (
            search,
            vec![
                r#"{"query":"a","query":"b"}"#,
                r#"{"query":1}"#,
                r#"{"query":"a","origin":"kumo"}"#,
                r#"{"query":"a""#,
            ],
        ),
        (
            read,
            vec![
                r#"{"task":2,"from":1,"to":2,"to":3}"#,
                r#"{"task":"2","from":1,"to":2}"#,
                r#"{"task":2,"from":1,"to":2,"x":1}"#,
            ],
        ),
        (
            tasks,
            vec![
                r#"{"x":1}"#,
                r#"{"kind":"tasks"}"#,
                "[]",
                "null",
                "{",
                r#"{"x":1,"x":2}"#,
            ],
        ),
    ] {
        for args in bad {
            assert!(
                matches!(prepare(args.as_bytes()), Err(ToolError::Invalid)),
                "args {args}"
            );
        }
    }
    assert!(matches!(
        tasks(b"{}"),
        Ok(Prepared::Read {
            query: Query::Tasks,
            ..
        })
    ));
}

#[test]
fn outer_history_carriers_redact_query_text_and_its_bytes() {
    let marker = "zqxhistoryprivate";
    let read = ReadCall {
        key: dispatch::LedgerKey {
            run: RunId(1),
            route: Route::Model,
            call: "id".into(),
        },
        build: BuildKey {
            run: RunId(1),
            round: 1,
            attempt: 1,
        },
        thread: ThreadId(2),
        tool: "search_history",
        hash: [0; 32],
        origin: Origin::Kumo,
        query: Query::Search {
            query: marker.into(),
            task: None,
        },
        control: Control::new(std::time::Instant::now()),
    };
    let done = ReadFinished {
        key: read.key.clone(),
        build: read.build,
        result: Ok((
            Selection::Search {
                query: marker.into(),
                hits: Vec::new(),
                more: false,
            },
            Vec::new(),
        )),
    };
    for debug in [
        format!("{:?}", crate::coordinator::StoreOp::ReadCall(read)),
        format!("{:?}", crate::coordinator::Input::ReadFinished(done)),
    ] {
        assert!(!debug.contains(marker));
        assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    }
}

#[test]
fn activation_offers_exact_ordered_model_schemas_and_no_mcp_history() {
    for (origin, names) in [
        (
            Origin::Kumo,
            vec![
                "draw",
                "draft_issue",
                "search_history",
                "read_history",
                "list_tasks",
            ],
        ),
        (
            Origin::Task(ThreadId(2)),
            vec!["draw", "draft_issue", "search_history", "read_history"],
        ),
    ] {
        assert_eq!(
            crate::context::tools(origin)
                .iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            names
        );
        for schema in crate::context::tools(origin) {
            assert_eq!(schema.parameters["additionalProperties"], false);
        }
        let mcp = dispatch::schemas(origin, Route::Mcp(crate::bridge::LaunchId(1)));
        assert_eq!(
            dispatch::mcp_schemas(origin)
                .iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            ["draw"]
        );
        assert_eq!(
            mcp.iter().filter(|tool| ["search_history", "read_history", "list_tasks"].contains(&tool.name)).count(),
            0
        );
    }
}

#[test]
fn reading_joins_canonical_retries_and_refusal_cancels_without_revival() {
    let mut ledger = Ledger::default();
    let run = RunId(1);
    let caller = Caller::Model {
        run,
        call: CallId::new("read").unwrap(),
    };
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ctx = dispatch::RunCtx {
        build: BuildKey {
            run,
            round: 1,
            attempt: 1,
        },
        end,
        control: Control::new(end),
        thread: ThreadId(2),
        main: ThreadId(1),
        fresh: ThreadId(3),
        reach: Origin::Task(ThreadId(2)),
        offered: &[ToolName::SearchHistory],
        mcp: None,
        ledger: &mut ledger,
    };
    let Step::Read(read) = dispatch::call(
        &mut ctx,
        caller.clone(),
        ToolName::SearchHistory,
        br#"{"query":"needle"}"#,
    ) else {
        panic!("the first call starts an asynchronous read");
    };
    assert_eq!(read.origin, Origin::Task(ThreadId(2)));
    assert_eq!(read.control.check(), Ok(()));
    assert!(matches!(
        dispatch::call(
            &mut ctx,
            caller.clone(),
            ToolName::SearchHistory,
            br#"{ "query": "needle", "task": null }"#
        ),
        Step::Join
    ));
    for args in [
        br#"{"query":"other"}"#.as_slice(),
        b"{",
        br#"{"query":"needle","origin":"kumo"}"#,
    ] {
        assert!(matches!(
            dispatch::call(&mut ctx, caller.clone(), ToolName::SearchHistory, args),
            Step::Answer(ToolOutput::Error(ToolError::Reused))
        ));
    }
    assert_eq!(
        ctx.ledger.refused(&read.key, ToolError::Ended),
        vec![caller.clone(), caller.clone()]
    );
    assert_eq!(read.control.check(), Err(Error::Ended));
    assert!(matches!(
        dispatch::call(
            &mut ctx,
            caller,
            ToolName::SearchHistory,
            br#"{"query":"needle"}"#
        ),
        Step::Answer(ToolOutput::Error(ToolError::Ended))
    ));
    assert_eq!(
        ctx.ledger.read_finished(
            &read.key,
            ToolOutput::Recall(Selection::Tasks {
                tasks: Vec::new(),
                truncated: false
            })
        ),
        None
    );
}
