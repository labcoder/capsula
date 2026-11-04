use std::path::Path;

use serde_json::json;

use super::*;

const TEXT: &[u8] = include_bytes!("../../../../fixtures/claude-code/text.jsonl");
const RETRY: &[u8] = include_bytes!("../../../../fixtures/claude-code/retry.jsonl");
const TURNS: &[u8] = include_bytes!("../../../../fixtures/claude-code/turns.jsonl");
const FACTS: &[u8] = include_bytes!("../../../../fixtures/claude-code/facts.jsonl");

/// Feeds `chunks` through one framer and one mapper, keeping everything but `Ignored`.
fn map(chunks: &[&[u8]]) -> (Result<Vec<Mapped>, ClaudeStreamFailure>, StreamMapper) {
    let mut framer = LineFramer::default();
    let mut mapper = StreamMapper::default();
    let mut out = Vec::new();
    let mut run = || -> Result<(), ClaudeStreamFailure> {
        for chunk in chunks {
            framer.push(chunk)?;
            while let Some(line) = framer.next_line()? {
                match mapper.line(line)? {
                    Mapped::Ignored => {}
                    other => out.push(other),
                }
            }
        }
        Ok(())
    };
    let result = run().map(|()| out);
    (result, mapper)
}

fn text(delta: &str) -> Mapped {
    Mapped::Event(TurnEvent::TextDelta(delta.to_owned()))
}

const ANSWER: Tokens = Tokens {
    input: 12,
    output: 7,
    cache_read: 4,
    cache_write: 3,
};

fn answered() -> Vec<Mapped> {
    vec![
        text("Hello from "),
        text("the fake. Caf\u{e9} \u{2713}"),
        Mapped::End {
            usage: Some(Usage {
                whole: ANSWER,
                first_request: Some(ANSWER),
            }),
            finish: Finish::Completed,
        },
    ]
}

/// Drops the leading `Init`, checking it's there.
fn after_init(mut mapped: Vec<Mapped>) -> Vec<Mapped> {
    assert!(
        matches!(mapped.first(), Some(Mapped::Init(_))),
        "{mapped:?}"
    );
    mapped.remove(0);
    mapped
}

#[test]
fn a_recorded_answer_yields_its_text_once_then_its_usage_and_end() {
    let (out, mapper) = map(&[TEXT]);
    assert_eq!(after_init(out.expect("maps")), answered());
    assert!(!mapper.policy_broken());
}

#[test]
fn a_retried_request_yields_one_native_turn_and_the_answer_once() {
    // Recorded with the first stream cut mid-message: Claude Code retried it and streamed only
    // the second attempt, announced by `system:api_retry`.
    let (out, mapper) = map(&[RETRY]);
    assert_eq!(after_init(out.expect("maps")), answered());
    assert!(!mapper.policy_broken());
}

#[test]
fn eight_turns_end_at_the_cap_without_breaking_the_policy() {
    let (out, mapper) = map(&[TURNS]);
    assert_eq!(
        after_init(out.expect("maps")),
        vec![Mapped::End {
            usage: Some(Usage {
                whole: Tokens {
                    input: 80,
                    output: 40,
                    cache_read: 0,
                    cache_write: 0,
                },
                first_request: Some(Tokens {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                }),
            }),
            finish: Finish::Failed(EngineFailure::TurnCap),
        }]
    );
    assert!(!mapper.policy_broken(), "the cap held");
}

#[test]
fn init_carries_the_audited_facts() {
    let (out, _) = map(&[TURNS]);
    let Some(Mapped::Init(init)) = out.expect("maps").into_iter().next() else {
        panic!("init first");
    };
    assert_eq!(
        init,
        InitFacts {
            claude_code_version: "2.1.306".to_owned(),
            tools: vec!["mcp__kumochi__ping".to_owned()],
            mcp_servers: vec![McpServer {
                name: "kumochi".to_owned(),
                status: "connected".to_owned()
            }],
            plugins: vec![
                Plugin {
                    name: "cc-plugin-agents-md".to_owned(),
                    path: "builtin".to_owned()
                },
                Plugin {
                    name: "cc-plugin-plugin-authoring".to_owned(),
                    path: "builtin".to_owned()
                },
            ],
            skills: vec![],
            slash_commands: vec![],
            api_key_source: "ANTHROPIC_API_KEY".to_owned(),
            agents: [
                "claude",
                "Explore",
                "general-purpose",
                "Plan",
                "statusline-setup"
            ]
            .map(str::to_owned)
            .to_vec(),
            analytics_disabled: Some(true),
        }
    );
}

#[test]
fn analytics_disabled_is_kept_as_reported_and_never_fails_init() {
    let init = |field: &str| {
        let line = format!(
            r#"{{"type":"system","subtype":"init","claude_code_version":"2.1.306","tools":[],"mcp_servers":[],"plugins":[],"skills":[],"slash_commands":[],"apiKeySource":"none","agents":[]{field}}}"#
        );
        match StreamMapper::default().line(line.as_bytes()) {
            Ok(Mapped::Init(facts)) => facts.analytics_disabled,
            other => panic!("{field}: {other:?}"),
        }
    };
    assert_eq!(init(r#","analytics_disabled":true"#), Some(true));
    assert_eq!(init(r#","analytics_disabled":false"#), Some(false));
    assert_eq!(init(""), None);
    assert_eq!(init(r#","analytics_disabled":"yes""#), None);
    assert_eq!(init(r#","analytics_disabled":null"#), None);
}

#[test]
fn control_responses_before_input_come_back_in_order() {
    let (out, _) = map(&[FACTS]);
    let out = out.expect("maps");
    let [Mapped::Control(init), Mapped::Control(mcp)] = out.as_slice() else {
        panic!("two control responses");
    };
    assert_eq!(
        (init.subtype.as_str(), init.request_id.as_str()),
        ("success", "p0-init")
    );
    let body = &init.response;
    assert_eq!(body["claude_code_version"], "2.1.306");
    assert_eq!(body["analytics_disabled"], true);
    assert_eq!(body["commands"], json!([]));
    assert_eq!(
        body["account"],
        json!({"tokenSource": "none", "apiProvider": "firstParty"})
    );
    let agents: Vec<&Value> = body["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .map(|agent| &agent["name"])
        .collect();
    assert_eq!(
        agents,
        [
            "claude",
            "Explore",
            "general-purpose",
            "Plan",
            "statusline-setup"
        ]
        .map(Value::from)
        .iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(
        (mcp.subtype.as_str(), mcp.request_id.as_str()),
        ("success", "p0-mcp")
    );
    assert_eq!(
        mcp.response,
        json!({"mcpServers": [{
            "name": "kumochi",
            "status": "connected",
            "serverInfo": {"name": "kumochi", "version": "0.0.0"},
            "config": {"type": "stdio", "command": "<path>", "args": ["<path>"]},
            "scope": "dynamic",
            "source": "dynamic",
            "tools": [{"name": "ping", "annotations": {}}]
        }]})
    );
}

#[test]
fn debug_shows_shapes_and_counts_never_stream_content() {
    const SENTINEL: &str = "zqxsentinel";
    let mut framer = LineFramer::default();
    framer
        .push(format!("{{\"type\":\"user\",\"x\":\"{SENTINEL}\"}}").as_bytes())
        .expect("push");
    let control = format!(
        r#"{{"type":"control_response","response":{{"subtype":"{SENTINEL}","request_id":"{SENTINEL}","response":{{"nested":{{"deep":["{SENTINEL}"]}}}}}}}}"#
    );
    let init = format!(
        r#"{{"type":"system","subtype":"init","claude_code_version":"{SENTINEL}","tools":["{SENTINEL}"],"mcp_servers":[{{"name":"{SENTINEL}","status":"{SENTINEL}"}}],"plugins":[{{"name":"{SENTINEL}","path":"{SENTINEL}"}}],"skills":["{SENTINEL}"],"slash_commands":["{SENTINEL}"],"apiKeySource":"{SENTINEL}","agents":["{SENTINEL}"],"analytics_disabled":false}}"#
    );
    let mut mapper = StreamMapper::default();
    let control = mapper.line(control.as_bytes()).expect("control");
    let init = mapper.line(init.as_bytes()).expect("init");
    let text = mapper
        .line(format!(r#"{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{SENTINEL}"}}}}}}"#).as_bytes())
        .expect("text");
    let output = format!("{framer:?} {control:?} {init:?} {text:?} {mapper:?}");
    let bytes: Vec<String> = SENTINEL.bytes().map(|byte| byte.to_string()).collect();
    assert!(!output.contains(SENTINEL), "{output}");
    assert!(!output.contains(&bytes.join(", ")), "{output}");
    assert_eq!(
        format!("{framer:?}"),
        "LineFramer { unread: 33, charged: 33, .. }"
    );
    assert_eq!(
        format!("{control:?}"),
        "Control(ControlResponse { subtype: len=11, request_id: len=11, response: object(len=1) })"
    );
    assert_eq!(
        format!("{init:?}"),
        "Init(InitFacts { claude_code_version: len=11, tools: 1, mcp_servers: [McpServer { name: len=11, status: len=11 }], plugins: [Plugin { name: len=11, path: len=11 }], skills: 1, slash_commands: 1, api_key_source: len=11, agents: 1, analytics_disabled: Some(false) })"
    );
}

#[test]
fn every_split_point_and_crlf_map_the_same() {
    let whole = answered();
    for at in 0..=TEXT.len() {
        let (out, _) = map(&[&TEXT[..at], &TEXT[at..]]);
        assert_eq!(after_init(out.expect("maps")), whole, "split at {at}");
    }
    let crlf: Vec<u8> = TEXT
        .iter()
        .flat_map(|&byte| {
            if byte == b'\n' {
                vec![b'\r', b'\n']
            } else {
                vec![byte]
            }
        })
        .collect();
    let (out, _) = map(&[&crlf]);
    assert_eq!(after_init(out.expect("maps")), whole);
}

fn start(n: u32) -> String {
    format!(
        r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"usage":{{"input_tokens":{n}}}}}}}}}"#
    )
}

#[test]
fn a_ninth_native_request_breaks_the_policy() {
    let eight = (1..=8).fold(String::new(), |mut lines, n| {
        lines.push_str(&start(n));
        lines.push('\n');
        lines
    });
    let (out, mapper) = map(&[eight.as_bytes()]);
    assert_eq!(out.expect("maps"), vec![]);
    assert!(!mapper.policy_broken());

    let result = r#"{"type":"result","subtype":"success","usage":{}}"#;
    let nine = format!("{eight}{}\n{result}\n", start(9));
    let (out, mapper) = map(&[nine.as_bytes()]);
    assert_eq!(
        out.expect("maps"),
        vec![Mapped::End {
            usage: None,
            finish: Finish::Failed(EngineFailure::TurnCap)
        }]
    );
    assert!(mapper.policy_broken());
}

#[test]
fn the_first_request_is_counted_apart_from_the_whole_question() {
    let lines = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        start(5),
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":2}}}"#,
        start(9),
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":4}}}"#,
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":14,"output_tokens":6}}"#,
    );
    let (out, _) = map(&[lines.as_bytes()]);
    assert_eq!(
        out.expect("maps"),
        vec![Mapped::End {
            usage: Some(Usage {
                whole: Tokens {
                    input: 14,
                    output: 6,
                    ..Tokens::default()
                },
                first_request: Some(Tokens {
                    input: 5,
                    output: 2,
                    ..Tokens::default()
                }),
            }),
            finish: Finish::Completed,
        }]
    );
}

#[test]
fn results_map_their_outcome_and_end_the_question() {
    for (line, finish) in [
        (
            r#"{"type":"result","subtype":"success","is_error":false}"#,
            Finish::Completed,
        ),
        (
            r#"{"type":"result","subtype":"success","is_error":true}"#,
            Finish::Failed(EngineFailure::Provider),
        ),
        (
            r#"{"type":"result","subtype":"error_during_execution"}"#,
            Finish::Failed(EngineFailure::Provider),
        ),
        (
            r#"{"type":"result","subtype":"error_max_turns"}"#,
            Finish::Failed(EngineFailure::TurnCap),
        ),
    ] {
        let mut mapper = StreamMapper::default();
        assert_eq!(
            mapper.line(line.as_bytes()),
            Ok(Mapped::End {
                usage: None,
                finish
            }),
            "{line}"
        );
        let late = br#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"late"}}}"#;
        assert_eq!(
            mapper.line(late),
            Ok(Mapped::Ignored),
            "text after the result"
        );
        assert_eq!(
            mapper.line(line.as_bytes()),
            Ok(Mapped::Ignored),
            "a second result"
        );
        assert_eq!(mapper.finish(), None, "no second Finished");
        assert!(!mapper.policy_broken());
    }
}

#[test]
fn bad_lines_fail_and_odd_ones_are_ignored() {
    let mut mapper = StreamMapper::default();
    assert_eq!(mapper.line(b""), Ok(Mapped::Ignored));
    assert_eq!(
        mapper.line(br#"{"type":"rate_limit_event"}"#),
        Ok(Mapped::Ignored)
    );
    assert_eq!(mapper.unknown(), 1);
    assert_eq!(mapper.line(b"not json"), Err(ClaudeStreamFailure::NotJson));
    assert_eq!(
        mapper.line(b"{\"type\":\"user\",\"x\":\"\xff\"}"),
        Err(ClaudeStreamFailure::NotJson)
    );
    assert_eq!(
        mapper.line(br#"{"kind":"user"}"#),
        Err(ClaudeStreamFailure::Mistyped)
    );
    assert_eq!(
        mapper.line(br#"{"type":"result","subtype":"success","usage":{"input_tokens":"many"}}"#),
        Err(ClaudeStreamFailure::Mistyped)
    );
    assert_eq!(
        mapper
            .line(br#"{"type":"result","subtype":"success","usage":{"input_tokens":4294967296}}"#),
        Err(ClaudeStreamFailure::Mistyped)
    );
    assert_eq!(
        mapper.line(br#"{"type":"system","subtype":"init","tools":[]}"#),
        Err(ClaudeStreamFailure::Mistyped)
    );
}

#[test]
fn eof_without_a_result_is_truncated_once() {
    let mut framer = LineFramer::default();
    framer.push(br#"{"type":"user"}"#).expect("push");
    assert_eq!(framer.next_line(), Ok(None));
    assert!(framer.has_partial(), "cut mid-line");
    let mut mapper = StreamMapper::default();
    assert_eq!(
        mapper.finish(),
        Some(TurnEvent::Finished(Finish::Truncated))
    );
    assert_eq!(mapper.finish(), None);
}

#[test]
fn a_line_of_exactly_the_bound_passes_and_one_more_byte_fails() {
    let mut line = vec![b'a'; MAX_LINE];
    line.push(b'\n');
    let mut framer = LineFramer::default();
    framer.push(&line).expect("push");
    assert_eq!(
        framer.next_line().map(|line| line.map(<[u8]>::len)),
        Ok(Some(MAX_LINE))
    );

    let mut over = vec![b'a'; MAX_LINE + 1];
    let mut framer = LineFramer::default();
    framer.push(&over).expect("push");
    assert_eq!(
        framer.next_line(),
        Err(ClaudeStreamFailure::LineTooLong),
        "fails before its newline"
    );
    over.push(b'\n');
    let mut framer = LineFramer::default();
    framer.push(&over).expect("push");
    assert_eq!(framer.next_line(), Err(ClaudeStreamFailure::LineTooLong));
}

#[test]
fn stdout_of_exactly_the_budget_passes_and_one_more_byte_fails() {
    let line = b"{\"type\":\"user\"}\n";
    let total = usize::try_from(MAX_STDOUT).expect("fits");
    let mut framer = LineFramer::default();
    let mut lines = 0;
    let mut sent = 0;
    while sent < total {
        let chunk = &line[..line.len().min(total - sent)];
        framer.push(chunk).expect("within the budget");
        sent += chunk.len();
        while let Some(_line) = framer.next_line().expect("frames") {
            lines += 1;
        }
    }
    assert_eq!(lines, total / line.len());
    assert_eq!(framer.push(b"x"), Err(ClaudeStreamFailure::Budget));
}

/// Why `text` can't sit in a committed recording: an absolute path in Windows, UNC or POSIX
/// spelling, `Users\`, or a UUID other than the all-zero placeholder. Checked on decoded JSON
/// strings, so escaped spellings count too.
fn leak(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    for at in 0..bytes.len() {
        let rest = &bytes[at..];
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let starts_token = before.is_none_or(|byte| !byte.is_ascii_alphanumeric());
        if starts_token
            && rest.len() >= 3
            && rest[0].is_ascii_alphabetic()
            && rest[1] == b':'
            && matches!(rest[2], b'\\' | b'/')
        {
            return Some("drive path");
        }
        if rest.starts_with(br"\\") {
            return Some("UNC path");
        }
        if rest.first() == Some(&b'/')
            && before.is_none_or(|byte| byte.is_ascii_whitespace() || b"\"'=(,".contains(&byte))
            && rest
                .get(1)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || b"~._".contains(byte))
        {
            return Some("rooted path");
        }
        if rest.starts_with(br"Users\") {
            return Some("Users\\");
        }
        let uuid = rest.get(..36).is_some_and(|window| {
            window.iter().enumerate().all(|(i, &byte)| match i {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
        });
        if uuid && &rest[..36] != b"00000000-0000-0000-0000-000000000000" {
            return Some("UUID");
        }
    }
    None
}

/// Every string in `value`, object keys included.
fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(fields) => {
            for (key, item) in fields {
                out.push(key);
                strings(item, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The first leak in a JSON line's decoded strings.
fn line_leak(line: &str) -> Option<&'static str> {
    let value: Value = serde_json::from_str(line).expect("a JSON line");
    let mut found = Vec::new();
    strings(&value, &mut found);
    found.into_iter().find_map(leak)
}

#[test]
fn the_leak_check_catches_paths_and_uuids_in_any_spelling() {
    for bad in [
        r#"{"cwd":"C:\\dev\\x"}"#,
        r#"{"cwd":"see c:/dev/x"}"#,
        r#"{"cwd":"C\u003a\\dev"}"#,
        r#"{"share":"\\\\server\\share"}"#,
        r#"{"home":"/home/example/private"}"#,
        r#"{"home":"\u002fhome\u002fexample"}"#,
        r#"{"args":["--config", "/etc/x"]}"#,
        r#"{"note":"run in /tmp now"}"#,
        r#"{"x":"Users\\someone"}"#,
        r#"{"session_id":"0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"}"#,
        r#"{"0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0":1}"#,
    ] {
        assert!(line_leak(bad).is_some(), "{bad}");
    }
    for fine in [
        r#"{"cwd":"<path>","path":"builtin"}"#,
        r#"{"session_id":"00000000-0000-0000-0000-000000000000"}"#,
        r#"{"url":"https://example.invalid/a/b","ratio":"1:2","pair":"a/b"}"#,
        r#"{"kind":"stdio","count":3}"#,
    ] {
        assert_eq!(line_leak(fine), None, "{fine}");
    }
}

#[test]
fn the_recorded_fixtures_hold_no_absolute_path_or_real_uuid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude-code");
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).expect("fixtures") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("UTF-8 recording");
        files += 1;
        for (number, line) in text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.is_empty())
        {
            assert_eq!(
                line_leak(line),
                None,
                "{} line {}",
                path.display(),
                number + 1
            );
        }
    }
    assert_eq!(files, 4);
}

/// The recorded text answer with its two text deltas repeated until the stream passes 1 MiB.
/// The same bytes feed `agent-spike`'s parser for the before number.
fn one_mib_stream() -> Vec<u8> {
    let lines: Vec<&[u8]> = TEXT
        .split(|&byte| byte == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = Vec::new();
    for line in &lines[..4] {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    while out.len() < 1 << 20 {
        for line in &lines[4..6] {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    for line in &lines[6..] {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// Before and after for the PR: `cargo test --release -p kumochi-brain --lib measure_claude_stream
/// -- --ignored --nocapture` on a quiet Rez under the lock.
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_claude_stream() {
    let bytes = one_mib_stream();
    let mut times = Vec::new();
    for _ in 0..200 {
        let started = std::time::Instant::now();
        let mut framer = LineFramer::default();
        let mut mapper = StreamMapper::default();
        let mut deltas = 0_u32;
        for chunk in bytes.chunks(64 * 1024) {
            framer.push(chunk).expect("within the budget");
            while let Some(line) = framer.next_line().expect("frames") {
                if let Mapped::Event(TurnEvent::TextDelta(_)) = mapper.line(line).expect("maps") {
                    deltas += 1;
                }
            }
        }
        times.push(started.elapsed());
        assert!(deltas > 1_000);
    }
    times.sort();
    println!(
        "claude stream: {} bytes, median {:?}, p95 {:?}",
        bytes.len(),
        times[times.len() / 2],
        times[times.len() * 95 / 100]
    );
}
