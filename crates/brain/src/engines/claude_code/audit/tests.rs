use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::super::policy::{REVIEWED_CLAUDE_CODE, Reviewed};
use super::super::stream::{LineFramer, Mapped, StreamMapper};
use super::*;

const FACTS: &[u8] = include_bytes!("../../../../fixtures/claude-code/facts.jsonl");
const TURNS: &[u8] = include_bytes!("../../../../fixtures/claude-code/turns.jsonl");

fn tools() -> BTreeSet<String> {
    ["mcp__kumochi__ping".to_owned()].into()
}

/// One mapped line through C6's framer and mapper.
fn mapped(line: &Value) -> Mapped {
    let mut framer = LineFramer::default();
    framer
        .push(format!("{line}\n").as_bytes())
        .expect("under budget");
    let line = framer.next_line().expect("a line").expect("complete");
    StreamMapper::default().line(line).expect("maps")
}

fn fixture_line(bytes: &[u8], pick: impl Fn(&Value) -> bool) -> Value {
    std::str::from_utf8(bytes)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("json"))
        .find(|line| pick(line))
        .expect("the fixture line")
}

/// P0's recorded answer to `initialize` (0) or `mcp_status` (1), with Kumochi's id and `edit`
/// applied to the line.
fn control(which: usize, edit: impl FnOnce(&mut Value)) -> ControlResponse {
    let id = [INITIALIZE_ID, MCP_STATUS_ID][which];
    let recorded = ["p0-init", "p0-mcp"][which];
    let mut line = fixture_line(FACTS, |line| line["response"]["request_id"] == recorded);
    line["response"]["request_id"] = json!(id);
    edit(&mut line);
    match mapped(&line) {
        Mapped::Control(response) => response,
        other => panic!("{other:?}"),
    }
}

/// 2.1.306's signed-in `account` (handoffs/CAPS-158/signin-followups.md), with made-up values.
fn signed_in(line: &mut Value) {
    line["response"]["response"]["account"] = json!({
        "email": "someone@example.com",
        "organization": "Example",
        "subscriptionType": "max",
        "apiProvider": "firstParty",
    });
}

fn audit() -> PreInputAudit {
    PreInputAudit::new(&REVIEWED_CLAUDE_CODE[0], tools())
}

#[test]
fn the_recorded_facts_pass_in_either_order() {
    let mut first = audit();
    assert_eq!(first.control(&control(0, signed_in)), Ok(Progress::Waiting));
    assert_eq!(first.control(&control(1, |_| {})), Ok(Progress::Ready));
    let mut second = audit();
    assert_eq!(second.control(&control(1, |_| {})), Ok(Progress::Waiting));
    assert_eq!(second.control(&control(0, signed_in)), Ok(Progress::Ready));
}

#[test]
fn a_signed_out_launch_is_signed_out_not_an_audit_failure() {
    assert_eq!(
        audit().control(&control(0, |_| {})),
        Ok(Progress::SignedOut),
        "P0's recording is signed out"
    );
}

#[test]
fn each_observed_account_shape_gets_its_verdict() {
    let shapes = [
        (
            "the stored sign-in, named",
            json!({"tokenSource": "claude.ai", "subscriptionType": "max", "apiProvider": "firstParty"}),
            Ok(Progress::Waiting),
        ),
        (
            "signed out",
            json!({"tokenSource": "none", "apiProvider": "firstParty"}),
            Ok(Progress::SignedOut),
        ),
        (
            "a key over the sign-in",
            json!({"tokenSource": "claude.ai", "apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"}),
            Err(AuditFailed::KeySource),
        ),
        (
            "a key while signed out",
            json!({"tokenSource": "none", "apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"}),
            Err(AuditFailed::KeySource),
        ),
        (
            "a key beside a subscription",
            json!({"subscriptionType": "max", "apiKeySource": "apiKeyHelper", "apiProvider": "firstParty"}),
            Err(AuditFailed::KeySource),
        ),
        (
            "bedrock",
            json!({"apiProvider": "bedrock"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "another provider beside a subscription",
            json!({"subscriptionType": "max", "apiProvider": "vertex"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "an auth token",
            json!({"tokenSource": "ANTHROPIC_AUTH_TOKEN", "apiProvider": "firstParty"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "another source beside a subscription",
            json!({"tokenSource": "CLAUDE_CODE_OAUTH_TOKEN", "subscriptionType": "max", "apiProvider": "firstParty"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "signed out beside a subscription",
            json!({"tokenSource": "none", "subscriptionType": "max", "apiProvider": "firstParty"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "no source and no subscription",
            json!({"apiProvider": "firstParty"}),
            Err(AuditFailed::TokenSource),
        ),
        (
            "no provider",
            json!({"subscriptionType": "max"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a mistyped provider",
            json!({"subscriptionType": "max", "apiProvider": 7}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a mistyped source",
            json!({"tokenSource": 7, "subscriptionType": "max", "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a null source beside a subscription",
            json!({"tokenSource": null, "subscriptionType": "max", "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a null source while signed out",
            json!({"tokenSource": null, "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a null subscription",
            json!({"subscriptionType": null, "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a numeric subscription",
            json!({"subscriptionType": 3, "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "an object subscription",
            json!({"subscriptionType": {"plan": "max"}, "apiProvider": "firstParty"}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a null provider",
            json!({"subscriptionType": "max", "apiProvider": null}),
            Err(AuditFailed::Malformed),
        ),
        (
            "a null key source beside a subscription",
            json!({"subscriptionType": "max", "apiKeySource": null, "apiProvider": "firstParty"}),
            Err(AuditFailed::KeySource),
        ),
        (
            "a numeric key source beside a subscription",
            json!({"subscriptionType": "max", "apiKeySource": 0, "apiProvider": "firstParty"}),
            Err(AuditFailed::KeySource),
        ),
    ];
    for (case, account, verdict) in shapes {
        let response = control(0, |line| line["response"]["response"]["account"] = account);
        assert_eq!(audit().control(&response), verdict, "{case}");
    }
}

fn initialize_fails(edit: impl FnOnce(&mut Value)) -> Result<Progress, AuditFailed> {
    audit().control(&control(0, |line| {
        signed_in(line);
        edit(&mut line["response"]["response"]);
    }))
}

type Case = (&'static str, fn(&mut Value), AuditFailed);

const INITIALIZE_CASES: &[Case] = &[
    (
        "another version",
        |body| body["claude_code_version"] = json!("2.1.307"),
        AuditFailed::Version,
    ),
    (
        "a four-part version",
        |body| body["claude_code_version"] = json!("2.1.306.1"),
        AuditFailed::Version,
    ),
    (
        "no version",
        |body| {
            body.as_object_mut()
                .expect("object")
                .remove("claude_code_version");
        },
        AuditFailed::Malformed,
    ),
    (
        "a numeric version",
        |body| body["claude_code_version"] = json!(2),
        AuditFailed::Malformed,
    ),
    (
        "a command",
        |body| body["commands"] = json!([{"name": "x"}]),
        AuditFailed::Commands,
    ),
    (
        "no commands",
        |body| {
            body.as_object_mut().expect("object").remove("commands");
        },
        AuditFailed::Malformed,
    ),
    (
        "an extra agent",
        |body| {
            body["agents"]
                .as_array_mut()
                .expect("array")
                .push(json!({"name": "x"}));
        },
        AuditFailed::Agents,
    ),
    (
        "a missing agent",
        |body| {
            body["agents"].as_array_mut().expect("array").pop();
        },
        AuditFailed::Agents,
    ),
    (
        "a renamed agent",
        |body| body["agents"][0]["name"] = json!("Claude"),
        AuditFailed::Agents,
    ),
    (
        "a duplicate agent",
        |body| {
            body["agents"]
                .as_array_mut()
                .expect("array")
                .push(json!({"name": "claude"}));
        },
        AuditFailed::Agents,
    ),
    (
        "a mistyped agent",
        |body| body["agents"][0] = json!("claude"),
        AuditFailed::Malformed,
    ),
    (
        "another token source",
        |body| body["account"]["tokenSource"] = json!("apiKey"),
        AuditFailed::TokenSource,
    ),
    (
        "no account",
        |body| {
            body.as_object_mut().expect("object").remove("account");
        },
        AuditFailed::Malformed,
    ),
];

#[test]
fn each_initialize_field_is_judged() {
    let cases = INITIALIZE_CASES;
    for &(case, edit, expected) in cases {
        assert_eq!(initialize_fails(edit), Err(expected), "{case}");
    }
    let mut reordered = audit();
    let shuffled = control(0, |line| {
        signed_in(line);
        line["response"]["response"]["agents"]
            .as_array_mut()
            .expect("array")
            .reverse();
    });
    assert_eq!(
        reordered.control(&shuffled),
        Ok(Progress::Waiting),
        "agents in any order"
    );
}

const MCP_STATUS_CASES: &[Case] = &[
    (
        "no server",
        |servers| *servers = json!([]),
        AuditFailed::McpServer,
    ),
    (
        "a second server",
        |servers| {
            let first = servers[0].clone();
            servers.as_array_mut().expect("array").push(first);
        },
        AuditFailed::McpServer,
    ),
    (
        "another name",
        |servers| servers[0]["name"] = json!("other"),
        AuditFailed::McpServer,
    ),
    (
        "failed",
        |servers| servers[0]["status"] = json!("failed"),
        AuditFailed::McpServer,
    ),
    (
        "an extra tool",
        |servers| {
            servers[0]["tools"]
                .as_array_mut()
                .expect("array")
                .push(json!({"name": "see"}));
        },
        AuditFailed::Tools,
    ),
    (
        "no tool",
        |servers| servers[0]["tools"] = json!([]),
        AuditFailed::Tools,
    ),
    (
        "a duplicate tool",
        |servers| {
            servers[0]["tools"]
                .as_array_mut()
                .expect("array")
                .push(json!({"name": "ping"}));
        },
        AuditFailed::Tools,
    ),
    (
        "no tools field",
        |servers| {
            servers[0].as_object_mut().expect("object").remove("tools");
        },
        AuditFailed::Malformed,
    ),
    (
        "mistyped servers",
        |servers| *servers = json!({}),
        AuditFailed::Malformed,
    ),
];

#[test]
fn each_mcp_status_field_is_judged() {
    let fails = |edit: fn(&mut Value)| {
        audit().control(&control(1, |line| {
            edit(&mut line["response"]["response"]["mcpServers"]);
        }))
    };
    let cases = MCP_STATUS_CASES;
    for &(case, edit, expected) in cases {
        assert_eq!(fails(edit), Err(expected), "{case}");
    }
}

#[test]
fn an_error_an_unknown_id_or_a_repeat_is_malformed() {
    let error = control(0, |line| line["response"]["subtype"] = json!("error"));
    assert_eq!(audit().control(&error), Err(AuditFailed::Malformed));
    let unknown = control(0, |line| line["response"]["request_id"] = json!("p0-init"));
    assert_eq!(audit().control(&unknown), Err(AuditFailed::Malformed));
    let mut twice = audit();
    assert_eq!(twice.control(&control(1, |_| {})), Ok(Progress::Waiting));
    assert_eq!(
        twice.control(&control(1, |_| {})),
        Err(AuditFailed::Malformed)
    );
}

#[test]
fn crlf_and_a_character_split_across_reads_reach_the_audit_whole() {
    let mut line = fixture_line(FACTS, |line| line["response"]["request_id"] == "p0-init");
    line["response"]["request_id"] = json!(INITIALIZE_ID);
    signed_in(&mut line);
    line["response"]["response"]["agents"][0]["description"] = json!("é");
    let bytes = format!("{line}\r\n").into_bytes();
    let split = bytes.iter().position(|&byte| byte == 0xC3).expect("the é") + 1;
    let mut framer = LineFramer::default();
    framer.push(&bytes[..split]).expect("under budget");
    assert_eq!(
        framer.next_line().expect("no error"),
        None,
        "half a line waits"
    );
    framer.push(&bytes[split..]).expect("under budget");
    let line = framer
        .next_line()
        .expect("no error")
        .expect("the line")
        .to_vec();
    let Mapped::Control(response) = StreamMapper::default().line(&line).expect("maps") else {
        panic!("not a control response");
    };
    assert_eq!(audit().control(&response), Ok(Progress::Waiting));
}

#[test]
fn the_control_requests_are_one_json_line_each_with_kumochis_ids() {
    let [initialize, mcp] = control_requests();
    for (line, id, subtype) in [
        (initialize, INITIALIZE_ID, "initialize"),
        (mcp, MCP_STATUS_ID, "mcp_status"),
    ] {
        assert!(!line[..line.len() - 1].contains(&b'\n'), "one line");
        assert_eq!(line.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(line).expect("json");
        assert_eq!(
            value,
            json!({"type": "control_request", "request_id": id, "request": {"subtype": subtype}})
        );
    }
}

/// P0's recorded `system:init` with `apiKeySource` as a subscription sign-in reports it, then `edit`.
fn init(edit: impl FnOnce(&mut Value)) -> InitFacts {
    let mut line = fixture_line(TURNS, |line| {
        line["type"] == "system" && line["subtype"] == "init"
    });
    line["apiKeySource"] = json!("none");
    edit(&mut line);
    match mapped(&line) {
        Mapped::Init(facts) => facts,
        other => panic!("{other:?}"),
    }
}

fn post() -> PostSeedAudit {
    PostSeedAudit::new(&REVIEWED_CLAUDE_CODE[0], tools())
}

#[test]
fn the_recorded_init_passes_and_its_analytics_flag_is_only_recorded() {
    assert_eq!(
        post().init(init(|_| {})),
        Ok(InitRecord {
            analytics_disabled: Some(true)
        })
    );
    let missing = init(|line| {
        line.as_object_mut()
            .expect("object")
            .remove("analytics_disabled");
    });
    assert_eq!(
        post().init(missing),
        Ok(InitRecord {
            analytics_disabled: None
        })
    );
    let off = init(|line| line["analytics_disabled"] = json!(false));
    assert_eq!(
        post().init(off),
        Ok(InitRecord {
            analytics_disabled: Some(false)
        })
    );
}

const INIT_CASES: &[Case] = &[
    (
        "another version",
        |line| line["claude_code_version"] = json!("2.1.305"),
        AuditFailed::Version,
    ),
    (
        "a built-in tool",
        |line| {
            line["tools"]
                .as_array_mut()
                .expect("array")
                .push(json!("Read"));
        },
        AuditFailed::Tools,
    ),
    (
        "the placeholder tool",
        |line| {
            line["tools"]
                .as_array_mut()
                .expect("array")
                .push(json!("DeferredToolPlaceholder"));
        },
        AuditFailed::Tools,
    ),
    (
        "no tools",
        |line| line["tools"] = json!([]),
        AuditFailed::Tools,
    ),
    (
        "a duplicate tool",
        |line| {
            line["tools"]
                .as_array_mut()
                .expect("array")
                .push(json!("mcp__kumochi__ping"));
        },
        AuditFailed::Tools,
    ),
    (
        "a failed server",
        |line| line["mcp_servers"][0]["status"] = json!("failed"),
        AuditFailed::McpServer,
    ),
    (
        "a second server",
        |line| {
            let first = line["mcp_servers"][0].clone();
            line["mcp_servers"]
                .as_array_mut()
                .expect("array")
                .push(first);
        },
        AuditFailed::McpServer,
    ),
    (
        "a plugin elsewhere",
        |line| line["plugins"][0]["path"] = json!(r"C:\elsewhere"),
        AuditFailed::Plugins,
    ),
    (
        "an extra plugin",
        |line| {
            line["plugins"]
                .as_array_mut()
                .expect("array")
                .push(json!({"name": "x", "path": "builtin"}));
        },
        AuditFailed::Plugins,
    ),
    (
        "a missing plugin",
        |line| {
            line["plugins"].as_array_mut().expect("array").pop();
        },
        AuditFailed::Plugins,
    ),
    (
        "a skill",
        |line| line["skills"] = json!(["x"]),
        AuditFailed::Skills,
    ),
    (
        "a slash command",
        |line| line["slash_commands"] = json!(["x"]),
        AuditFailed::Skills,
    ),
    (
        "an extra agent",
        |line| {
            line["agents"]
                .as_array_mut()
                .expect("array")
                .push(json!("x"));
        },
        AuditFailed::Agents,
    ),
    (
        "the user's key",
        |line| line["apiKeySource"] = json!("user"),
        AuditFailed::KeySource,
    ),
    (
        "P0's placeholder key",
        |line| line["apiKeySource"] = json!("ANTHROPIC_API_KEY"),
        AuditFailed::KeySource,
    ),
];

#[test]
fn each_init_field_is_judged() {
    let cases = INIT_CASES;
    for &(case, edit, expected) in cases {
        assert_eq!(post().init(init(edit)), Err(expected), "{case}");
    }
}

#[test]
fn a_later_init_must_equal_the_first() {
    let mut audit = post();
    assert!(audit.init(init(|_| {})).is_ok(), "the first passes");
    assert_eq!(
        audit.init(init(|_| {})),
        Ok(InitRecord {
            analytics_disabled: Some(true)
        })
    );
    assert_eq!(
        audit.init(init(|line| line["skills"] = json!(["x"]))),
        Err(AuditFailed::Malformed)
    );
    let mut other = post();
    assert!(other.init(init(|_| {})).is_ok(), "the first passes");
    let renamed = init(|line| line["plugins"].as_array_mut().expect("array").reverse());
    assert_eq!(
        other.init(renamed),
        Err(AuditFailed::Malformed),
        "a reordered list differs from the first"
    );
}

#[test]
fn the_gate_starts_pending_and_failure_is_final() {
    let gate = AuditGate::new();
    let listener = gate.clone();
    assert!(!listener.may_call(), "pending");
    gate.pass();
    assert!(listener.may_call(), "passed");
    gate.fail();
    assert!(!listener.may_call(), "revoked");
    gate.pass();
    assert!(!listener.may_call(), "a pass after failure loses");
    let early = AuditGate::new();
    early.fail();
    early.pass();
    assert!(!early.may_call(), "a pass that lost the race to a failure");
    assert_eq!(format!("{early:?}"), "AuditGate(\"Failed\")");
}

#[test]
fn a_refusal_stands_against_every_later_response() {
    let real = &REVIEWED_CLAUDE_CODE[0];
    let refusals: [(
        &str,
        &'static Reviewed,
        ControlResponse,
        Result<Progress, AuditFailed>,
    ); 5] = [
        (
            "signed out",
            real,
            control(0, |_| {}),
            Ok(Progress::SignedOut),
        ),
        (
            "a key over the sign-in",
            real,
            control(0, |line| {
                line["response"]["response"]["account"]["apiKeySource"] =
                    json!("ANTHROPIC_API_KEY");
            }),
            Err(AuditFailed::KeySource),
        ),
        (
            "another version",
            &REVIEWED_CLAUDE_CODE[0],
            control(0, |line| {
                signed_in(line);
                line["response"]["response"]["claude_code_version"] = json!("2.1.307");
            }),
            Err(AuditFailed::Version),
        ),
        (
            "an error response",
            &REVIEWED_CLAUDE_CODE[0],
            control(0, |line| line["response"]["subtype"] = json!("error")),
            Err(AuditFailed::Malformed),
        ),
        (
            "a failed server",
            &REVIEWED_CLAUDE_CODE[0],
            control(1, |line| {
                line["response"]["response"]["mcpServers"][0]["status"] = json!("failed");
            }),
            Err(AuditFailed::McpServer),
        ),
    ];
    for (case, row, refusing, refusal) in refusals {
        let mut audit = PreInputAudit::new(row, tools());
        assert_eq!(audit.control(&refusing), refusal, "{case}");
        for valid in [
            control(1, |_| {}),
            control(0, signed_in),
            control(1, |_| {}),
        ] {
            assert_eq!(
                audit.control(&valid),
                refusal,
                "{case}: a valid response afterwards"
            );
        }
    }
}

#[test]
fn the_callers_tool_set_is_what_both_audits_expect() {
    let wider: BTreeSet<String> = ["mcp__kumochi__ping", "mcp__kumochi__see"]
        .map(String::from)
        .into();
    for expected in [
        wider,
        BTreeSet::new(),
        ["mcp__kumochi__see".to_owned()].into(),
    ] {
        let mut before = PreInputAudit::new(&REVIEWED_CLAUDE_CODE[0], expected.clone());
        assert_eq!(
            before.control(&control(1, |_| {})),
            Err(AuditFailed::Tools),
            "{expected:?}"
        );
        let mut after = PostSeedAudit::new(&REVIEWED_CLAUDE_CODE[0], expected.clone());
        assert_eq!(
            after.init(init(|_| {})),
            Err(AuditFailed::Tools),
            "{expected:?}"
        );
    }
}
