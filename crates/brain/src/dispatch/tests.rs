use kumochi_protocol::message::{EventId, RunId, ThreadId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Caller, CommitCall, Dispatch, Ledger, LedgerKey, LocalWrite, MAX_ARGS, MAX_OUTPUT, Prepared,
    Route, RunCtx, Step, Tool, ToolError, ToolOutput, call, parameters, parse, schemas,
};
use crate::bridge::{CallKey, LaunchId};
use crate::context::{HistoryRef, Origin};
use crate::engines::{CallId, ToolName};

const R: RunId = RunId(10);
const T: ThreadId = ThreadId(4);
const L1: LaunchId = LaunchId(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
const L2: LaunchId = LaunchId(2);
const MARKER: &str = "zqxplantedmarker";

/// The test tool that may write: renames the run's thread when `title` is set.
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProbeArgs {
    word: String,
    #[serde(default)]
    title: Option<String>,
    /// How many times the output repeats `word`.
    #[serde(default = "one")]
    count: u32,
}

fn one() -> u32 {
    1
}

fn prepare_probe(args: &[u8]) -> Result<Prepared, ToolError> {
    let (probe, hash) = parse::<ProbeArgs>("probe", args)?;
    let repeats = usize::try_from(probe.count).map_err(|_| ToolError::Invalid)?;
    Ok(Prepared {
        hash,
        write: probe.title.map_or(LocalWrite::None, LocalWrite::Title),
        output: ToolOutput::Text(format!("probed {}", probe.word.repeat(repeats))),
    })
}

pub(super) const PROBE: Tool = Tool {
    name: ToolName::Probe,
    wire: "probe",
    description: "A test tool.",
    kumo_only: false,
    mcp: true,
    parameters: parameters::<ProbeArgs>,
    prepare: prepare_probe,
};

/// The test history tool: Kumo only, model only.
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecallArgs {
    task: u64,
    first: u64,
    last: u64,
}

fn prepare_recall(args: &[u8]) -> Result<Prepared, ToolError> {
    let (recall, hash) = parse::<RecallArgs>("recall", args)?;
    Ok(Prepared {
        hash,
        write: LocalWrite::None,
        output: ToolOutput::History(vec![HistoryRef {
            thread: ThreadId(recall.task),
            first: EventId(recall.first),
            last: EventId(recall.last),
        }]),
    })
}

pub(super) const RECALL: Tool = Tool {
    name: ToolName::Recall,
    wire: "recall",
    description: "A test history tool.",
    kumo_only: true,
    mcp: false,
    parameters: parameters::<RecallArgs>,
    prepare: prepare_recall,
};

const OFFERED: &[ToolName] = &[ToolName::Probe, ToolName::Recall];

fn ctx(ledger: &mut Ledger, reach: Origin, mcp: Option<LaunchId>) -> RunCtx<'_> {
    RunCtx {
        thread: T,
        reach,
        offered: OFFERED,
        mcp,
        ledger,
    }
}

fn model(id: &str) -> Caller {
    Caller::Model {
        run: R,
        call: CallId::new(id).expect("a short ASCII id"),
    }
}

fn mcp(launch: LaunchId, key: &str) -> Caller {
    Caller::Mcp {
        run: R,
        launch,
        key: CallKey(key.to_owned()),
    }
}

/// One call by `caller` in a task run whose launch L1 is authorized.
fn send(ledger: &mut Ledger, caller: Caller, tool: ToolName, args: &str) -> Step {
    call(
        &mut ctx(ledger, Origin::Task(T), Some(L1)),
        caller,
        tool,
        args.as_bytes(),
    )
}

fn error(error: ToolError) -> Step {
    Step::Answer(ToolOutput::Error(error))
}

fn text(text: &str) -> ToolOutput {
    ToolOutput::Text(text.to_owned())
}

/// The write a step sends, or a panic naming what it was instead.
fn commit(step: Step) -> CommitCall {
    match step {
        Step::Commit(call) => call,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Steps compare by what a caller would see: an answer's output, a join, or a commit's key.
fn same(step: &Step, expected: &Step) -> bool {
    match (step, expected) {
        (Step::Answer(a), Step::Answer(b)) => a == b,
        (Step::Join, Step::Join) => true,
        (Step::Commit(a), Step::Commit(b)) => a == b,
        _ => false,
    }
}

#[track_caller]
fn assert_step(step: &Step, expected: &Step) {
    assert!(same(step, expected), "{step:?} != {expected:?}");
}

#[test]
fn a_first_valid_call_sends_its_write_its_output_and_its_key() {
    let mut ledger = Ledger::default();
    let sent = commit(send(
        &mut ledger,
        model("c1"),
        ToolName::Probe,
        r#"{"word":"hi","title":"Renamed"}"#,
    ));
    assert_eq!(
        sent.key,
        LedgerKey {
            run: R,
            route: Route::Model,
            call: "c1".into()
        }
    );
    assert_eq!(
        (sent.thread, sent.tool, &sent.write, &sent.output),
        (
            T,
            "probe",
            &LocalWrite::Title("Renamed".to_owned()),
            &text("probed hi")
        )
    );
}

#[test]
fn a_duplicate_while_running_joins_and_after_the_ack_gets_the_stored_output() {
    let mut ledger = Ledger::default();
    let first = commit(send(
        &mut ledger,
        model("c1"),
        ToolName::Probe,
        r#"{"word":"hi"}"#,
    ));
    assert_step(
        &send(
            &mut ledger,
            model("c1"),
            ToolName::Probe,
            r#"{"word":"hi"}"#,
        ),
        &Step::Join,
    );
    assert_eq!(
        ledger.committed(&first.key),
        vec![
            (model("c1"), text("probed hi")),
            (model("c1"), text("probed hi"))
        ],
        "the first caller and the one that joined"
    );
    assert_step(
        &send(
            &mut ledger,
            model("c1"),
            ToolName::Probe,
            r#"{"word":"hi"}"#,
        ),
        &Step::Answer(text("probed hi")),
    );
    assert_eq!(
        ledger.committed(&first.key),
        [],
        "a second ack answers nobody"
    );
}

#[test]
fn whitespace_key_order_and_an_omitted_default_hash_the_same_and_a_new_value_does_not() {
    let mut ledger = Ledger::default();
    commit(send(
        &mut ledger,
        model("c1"),
        ToolName::Probe,
        r#"{"word":"hi","count":1}"#,
    ));
    for same_call in [
        r#"{ "count" : 1 , "word" : "hi" }"#,
        r#"{"word":"hi"}"#,
        r#"{"word":"hi","title":null}"#,
    ] {
        assert_step(
            &send(&mut ledger, model("c1"), ToolName::Probe, same_call),
            &Step::Join,
        );
    }
    for other_call in [r#"{"word":"ho"}"#, r#"{"word":"hi","count":2}"#] {
        assert_step(
            &send(&mut ledger, model("c1"), ToolName::Probe, other_call),
            &error(ToolError::Reused),
        );
    }
    assert_step(
        &send(
            &mut ledger,
            model("c1"),
            ToolName::Probe,
            r#"{"word":"hi"}"#,
        ),
        &Step::Join,
    );
}

#[test]
fn another_tool_under_the_same_key_is_refused_and_leaves_the_first_call() {
    let mut ledger = Ledger::default();
    let first = commit(send(
        &mut ledger,
        model("c1"),
        ToolName::Probe,
        r#"{"word":"hi"}"#,
    ));
    let recall = r#"{"task":5,"first":1,"last":2}"#;
    let kumo = |ledger: &mut Ledger| {
        call(
            &mut ctx(ledger, Origin::Kumo, None),
            model("c1"),
            ToolName::Recall,
            recall.as_bytes(),
        )
    };
    assert_step(&kumo(&mut ledger), &error(ToolError::Reused));
    assert_eq!(
        ledger.committed(&first.key),
        [(model("c1"), text("probed hi"))]
    );
    assert_step(&kumo(&mut ledger), &error(ToolError::Reused));
}

#[test]
fn a_refused_key_stays_refused_and_a_new_key_runs() {
    let mut ledger = Ledger::default();
    let oversized = format!(r#"{{"word":"{}"}}"#, "x".repeat(MAX_ARGS));
    let valid = r#"{"word":"hi"}"#;
    for (args, refusal) in [
        (r#"{"word":"hi","extra":1}"#, ToolError::Invalid),
        (oversized.as_str(), ToolError::Invalid),
        (valid, ToolError::Invalid),
    ] {
        assert_step(
            &send(&mut ledger, model("bad"), ToolName::Probe, args),
            &error(refusal),
        );
    }
    for args in [
        r#"["hi",null,1]"#,
        r#"  ["hi"]"#,
        r#""hi""#,
        "1",
        "null",
        "",
    ] {
        let mut arrays = Ledger::default();
        for args in [args, valid] {
            assert_step(
                &send(&mut arrays, model("array"), ToolName::Probe, args),
                &error(ToolError::Invalid),
            );
        }
    }
    let mut big = Ledger::default();
    for args in [oversized.as_str(), r#"{"word":"#, valid] {
        assert_step(
            &send(&mut big, model("big"), ToolName::Probe, args),
            &error(ToolError::TooLarge),
        );
    }
    commit(send(&mut ledger, model("good"), ToolName::Probe, valid));
}

#[test]
fn malformed_duplicates_never_change_a_running_or_finished_call() {
    let mut ledger = Ledger::default();
    let first = commit(send(
        &mut ledger,
        model("c1"),
        ToolName::Probe,
        r#"{"word":"hi"}"#,
    ));
    let oversized = format!(r#"{{"word":"{}"}}"#, "x".repeat(MAX_ARGS));
    let malformed = [r#"{"word":"hi","extra":1}"#, "{", "", oversized.as_str()];
    for args in malformed {
        assert_step(
            &send(&mut ledger, model("c1"), ToolName::Probe, args),
            &error(ToolError::Reused),
        );
    }
    assert_step(
        &send(
            &mut ledger,
            model("c1"),
            ToolName::Probe,
            r#"{"word":"hi"}"#,
        ),
        &Step::Join,
    );
    assert_eq!(ledger.committed(&first.key).len(), 2);
    for args in malformed {
        assert_step(
            &send(&mut ledger, model("c1"), ToolName::Probe, args),
            &error(ToolError::Reused),
        );
    }
    assert_step(
        &send(
            &mut ledger,
            model("c1"),
            ToolName::Probe,
            r#"{"word":"hi"}"#,
        ),
        &Step::Answer(text("probed hi")),
    );
}

#[test]
fn arguments_up_to_the_cap_parse_and_one_byte_over_is_too_large() {
    let body = r#"{"word":"hi"}"#;
    let at_cap = format!("{body}{}", " ".repeat(MAX_ARGS - body.len()));
    let over = format!("{at_cap} ");
    let mut ledger = Ledger::default();
    commit(send(&mut ledger, model("at"), ToolName::Probe, &at_cap));
    assert_step(
        &send(&mut ledger, model("over"), ToolName::Probe, &over),
        &error(ToolError::TooLarge),
    );
}

#[test]
fn an_output_of_exactly_its_cap_is_sent_and_one_byte_over_is_refused() {
    assert_eq!(MAX_OUTPUT, 65_536);
    // `{"text":"probed "}` is 18 bytes, and the two-byte word checks bytes, not characters.
    for (word, count) in [("a", 65_518), ("\u{e9}", 32_759)] {
        let mut ledger = Ledger::default();
        let args = format!(r#"{{"word":"{word}","count":{count}}}"#);
        let sent = commit(send(&mut ledger, model("fits"), ToolName::Probe, &args));
        assert_eq!(sent.output.stored().len(), 65_536);
    }
    for (word, count) in [("a", 65_519), ("\u{e9}", 32_760)] {
        let mut ledger = Ledger::default();
        let args = format!(r#"{{"word":"{word}","count":{count}}}"#);
        assert_step(
            &send(&mut ledger, model("over"), ToolName::Probe, &args),
            &error(ToolError::TooLarge),
        );
    }
}

#[test]
fn the_same_id_on_each_route_and_under_each_launch_is_a_different_call() {
    let mut ledger = Ledger::default();
    let args = r#"{"word":"hi"}"#;
    let by_model = commit(send(&mut ledger, model("c1"), ToolName::Probe, args));
    let by_l1 = commit(send(&mut ledger, mcp(L1, "c1"), ToolName::Probe, args));
    let by_l2 = commit(call(
        &mut ctx(&mut ledger, Origin::Task(T), Some(L2)),
        mcp(L2, "c1"),
        ToolName::Probe,
        args.as_bytes(),
    ));
    assert_eq!(
        [by_model.key.route, by_l1.key.route, by_l2.key.route],
        [Route::Model, Route::Mcp(L1), Route::Mcp(L2)]
    );
    assert_eq!(
        [Route::Model.column(), Route::Mcp(L1).column()],
        ["model", "mcp:0123456789abcdef0123456789abcdef"]
    );
}

#[test]
fn a_launch_that_isnt_authorized_is_refused_without_using_up_its_key() {
    let mut ledger = Ledger::default();
    let args = r#"{"word":"hi"}"#;
    for authorized in [None, Some(L2)] {
        let step = call(
            &mut ctx(&mut ledger, Origin::Task(T), authorized),
            mcp(L1, "k"),
            ToolName::Probe,
            args.as_bytes(),
        );
        assert_step(&step, &error(ToolError::NotHere));
    }
    commit(send(&mut ledger, mcp(L1, "k"), ToolName::Probe, args));
}

#[test]
fn history_is_kumo_only_and_model_only() {
    let recall = r#"{"task":5,"first":7,"last":9}"#;
    let mut ledger = Ledger::default();
    let from = |ledger: &mut Ledger, reach, caller| {
        call(
            &mut ctx(ledger, reach, Some(L1)),
            caller,
            ToolName::Recall,
            recall.as_bytes(),
        )
    };
    assert_step(
        &from(&mut ledger, Origin::Kumo, mcp(L1, "m")),
        &error(ToolError::NotHere),
    );
    assert_step(
        &from(&mut ledger, Origin::Kumo, mcp(L1, "m")),
        &error(ToolError::NotHere),
    );
    assert_step(
        &from(&mut ledger, Origin::Task(T), model("t")),
        &error(ToolError::NotHere),
    );
    let sent = commit(from(&mut ledger, Origin::Kumo, model("k")));
    assert_eq!(
        sent.output,
        ToolOutput::History(vec![HistoryRef {
            thread: ThreadId(5),
            first: EventId(7),
            last: EventId(9),
        }])
    );
    let names = |reach, route| -> Vec<&str> {
        schemas(reach, route)
            .iter()
            .map(|schema| schema.name)
            .collect()
    };
    assert_eq!(names(Origin::Kumo, Route::Model), ["probe", "recall"]);
    assert_eq!(names(Origin::Kumo, Route::Mcp(L1)), ["probe"]);
    assert_eq!(names(Origin::Task(T), Route::Model), ["probe"]);
}

#[test]
fn an_unknown_or_unoffered_tool_is_refused_and_stays_refused() {
    let mut ledger = Ledger::default();
    let args = r#"{"word":"hi"}"#;
    assert_step(
        &send(&mut ledger, model("u"), ToolName::Unknown, args),
        &error(ToolError::Unknown),
    );
    assert_step(
        &send(&mut ledger, model("u"), ToolName::Probe, args),
        &error(ToolError::Unknown),
    );
    let unoffered = call(
        &mut RunCtx {
            thread: T,
            reach: Origin::Task(T),
            offered: &[],
            mcp: None,
            ledger: &mut ledger,
        },
        model("n"),
        ToolName::Probe,
        args.as_bytes(),
    );
    assert_step(&unoffered, &error(ToolError::NotHere));
    assert_step(
        &send(&mut ledger, model("n"), ToolName::Probe, args),
        &error(ToolError::NotHere),
    );
}

#[test]
fn a_failed_write_answers_nobody_and_its_key_stays_refused() {
    let mut ledger = Ledger::default();
    let args = r#"{"word":"hi"}"#;
    let first = commit(send(&mut ledger, model("c1"), ToolName::Probe, args));
    assert_step(
        &send(&mut ledger, model("c1"), ToolName::Probe, args),
        &Step::Join,
    );
    ledger.failed(&first.key);
    assert_step(
        &send(&mut ledger, model("c1"), ToolName::Probe, args),
        &error(ToolError::Ended),
    );
}

#[test]
fn outputs_round_trip_through_their_stored_json() {
    let refs = vec![HistoryRef {
        thread: ThreadId(5),
        first: EventId(7),
        last: EventId(9),
    }];
    let cases = [
        (text("ok"), r#"{"text":"ok"}"#),
        (
            ToolOutput::History(refs),
            r#"{"history":[{"first":7,"last":9,"thread":5}]}"#,
        ),
        (
            ToolOutput::Error(ToolError::Unknown),
            r#"{"error":"unknown tool"}"#,
        ),
    ];
    for (output, stored) in cases {
        assert_eq!(
            serde_json::from_str::<Value>(&output.stored()).expect("JSON"),
            serde_json::from_str::<Value>(stored).expect("JSON")
        );
        assert_eq!(ToolOutput::from_stored(stored), Some(output));
    }
    for bad in [
        r#"{"text":"ok","error":"unknown tool"}"#,
        r#"{"error":"something else"}"#,
        r#"{"history":[{"thread":5,"first":9,"last":7}]}"#,
        r#"{"history":[{"thread":5,"first":7,"last":9,"text":"x"}]}"#,
        r#"{"other":1}"#,
        "[]",
        "",
    ] {
        assert_eq!(ToolOutput::from_stored(bad), None, "{bad}");
    }
}

#[test]
fn each_schema_is_closed_and_names_its_fields_and_types() {
    let probe = parameters::<ProbeArgs>();
    assert_eq!(probe["additionalProperties"], json!(false));
    assert_eq!(probe["required"], json!(["word"]));
    assert_eq!(probe["properties"]["word"]["type"], json!("string"));
    assert_eq!(probe["properties"]["count"]["type"], json!("integer"));
    let recall = parameters::<RecallArgs>();
    assert_eq!(recall["additionalProperties"], json!(false));
    let mut required: Vec<&str> = recall["required"]
        .as_array()
        .expect("a required list")
        .iter()
        .filter_map(|name| name.as_str())
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["first", "last", "task"]);
    for (args, parses) in [
        (r#"{"word":"hi","count":2}"#, true),
        (r#"{"count":2}"#, false),
        (r#"{"word":"hi","count":"2"}"#, false),
        (r#"{"word":"hi","more":1}"#, false),
    ] {
        assert_eq!(prepare_probe(args.as_bytes()).is_ok(), parses, "{args}");
    }
}

#[test]
fn diagnostics_never_show_arguments_outputs_ids_or_titles() {
    let mut ledger = Ledger::default();
    let args = format!(r#"{{"word":"{MARKER}","title":"{MARKER}"}}"#);
    let sent = commit(send(&mut ledger, model(MARKER), ToolName::Probe, &args));
    let formatted = [
        format!("{:?}", model(MARKER)),
        format!("{:?}", mcp(L1, MARKER)),
        format!("{sent:?}"),
        format!(
            "{:?}",
            crate::coordinator::StoreOp::CommitCall(sent.clone())
        ),
        format!(
            "{:?}",
            crate::coordinator::Input::CallCommitted {
                key: sent.key,
                committed: true
            }
        ),
        format!("{:?}", Dispatch::Answered(text(MARKER))),
        format!("{:?}", Step::Answer(text(MARKER))),
        format!("{:?}", LocalWrite::Title(MARKER.to_owned())),
        format!("{ledger:?}"),
    ];
    let listed = format!("{:?}", MARKER.as_bytes());
    let bytes = listed.trim_matches(['[', ']']);
    for shown in formatted {
        assert!(!shown.contains(MARKER), "{shown}");
        assert!(!shown.contains(bytes), "{shown}");
    }
}
