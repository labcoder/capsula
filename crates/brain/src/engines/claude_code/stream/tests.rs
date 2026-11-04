use std::path::Path;

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
    let events = after_init(out.expect("maps"));
    assert!(
        matches!(
            events.as_slice(),
            [Mapped::End {
                usage: Some(_),
                finish: Finish::Failed(EngineFailure::TurnCap)
            }]
        ),
        "{events:?}"
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
        }
    );
}

#[test]
fn control_responses_before_input_come_back_in_order() {
    let (out, _) = map(&[FACTS]);
    let ids: Vec<(String, String)> = out
        .expect("maps")
        .into_iter()
        .map(|mapped| match mapped {
            Mapped::Control(control) => (control.subtype, control.request_id),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        ids,
        [("success", "p0-init"), ("success", "p0-mcp")].map(|(a, b)| (a.to_owned(), b.to_owned()))
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

/// True when `text` has a 36-byte UUID at `at`.
fn uuid_at(text: &[u8], at: usize) -> bool {
    text.get(at..at + 36).is_some_and(|window| {
        window.iter().enumerate().all(|(i, &byte)| match i {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
    })
}

#[test]
fn the_recorded_fixtures_hold_no_absolute_path_or_real_uuid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude-code");
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).expect("fixtures") {
        let path = entry.expect("entry").path();
        let text = std::fs::read(&path).expect("read");
        files += 1;
        for at in 0..text.len() {
            let rest = &text[at..];
            let drive = rest.len() >= 3
                && rest[0].is_ascii_alphabetic()
                && rest[1] == b':'
                && (rest[2] == b'\\' || rest[2] == b'/')
                && (at == 0 || !text[at - 1].is_ascii_alphanumeric());
            assert!(!drive, "{}: absolute path at {at}", path.display());
            assert!(
                !rest.starts_with(br"\\\\"),
                "{}: UNC path at {at}",
                path.display()
            );
            assert!(
                !rest.starts_with(br"Users\"),
                "{}: Users\\ at {at}",
                path.display()
            );
            if uuid_at(&text, at) {
                assert_eq!(
                    &text[at..at + 36],
                    b"00000000-0000-0000-0000-000000000000",
                    "{}",
                    path.display()
                );
            }
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
